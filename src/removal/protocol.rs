//! Reversible removal of one linked worktree.
//!
//! The directory is first moved aside (no-replace rename) and checked
//! there; only after Git has dropped the registration is it deleted. Every
//! failure before that puts the directory back. Each step is journaled
//! before the next starts, so a crash anywhere can be recovered.

use std::io;
use std::path::{Path, PathBuf};

use crate::core::facts::DirState;
use crate::core::ids::{YardPath, now};
use crate::fs::dirstate::dir_state;
use crate::fs::rename::rename_noreplace;
use crate::fs::tree_scan;
use crate::git::cli;
use crate::git::open::Registry;
use crate::store::journal::{Entry, Journal, Phase, Quarantine};
use crate::store::lock::RepoLocks;

use super::request::{Outcome, Request};
use super::validate::{Validation, path_absent, registration_exists, validate};

/// Points where tests can inject a crash or a failing journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Intent,
    Rename,
    Quarantined,
    Scan,
    GitRemove,
    RegistrationRemoved,
    Delete,
    Deleted,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injected {
    /// Stop dead, as if the process was killed before `Step`.
    Crash,
    /// The journal write of `Step` fails.
    JournalError,
}

pub struct Context<'a> {
    pub registry: &'a Registry,
    pub journal: &'a Journal,
    pub locks: &'a RepoLocks,
    /// Checked before anything is changed.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Reports that validation passed and changes are starting.
    pub on_removing: &'a (dyn Fn() + Sync),
    pub probe: &'a (dyn Fn(Step) -> Option<Injected> + Sync),
}

pub fn no_probe(_: Step) -> Option<Injected> {
    None
}

/// The outcome, or `None` when a probe simulated a crash.
pub fn remove(context: &Context<'_>, request: &Request) -> Option<Outcome> {
    let repo = request.key.repo();
    let _guard = match context.locks.lock(&repo) {
        Ok(guard) => guard,
        Err(error) => {
            return Some(Outcome::Blocked {
                reason: format!("{error:#}"),
            });
        }
    };
    if (context.cancelled)() {
        return Some(Outcome::Cancelled);
    }
    match validate(context.registry, request) {
        Ok(_) => {}
        Err(Validation::Blocked(reason)) => return Some(Outcome::Blocked { reason }),
        Err(Validation::AlreadyGone) => return Some(Outcome::AlreadyGone),
    }
    if (context.cancelled)() {
        return Some(Outcome::Cancelled);
    }
    (context.on_removing)();
    Run::new(context, request).run()
}

struct Run<'a> {
    context: &'a Context<'a>,
    request: &'a Request,
    op: String,
    warnings: Vec<String>,
}

/// A probe simulated the process dying.
struct Crashed;

impl<'a> Run<'a> {
    fn new(context: &'a Context<'a>, request: &'a Request) -> Self {
        Self {
            op: context.journal.new_op(),
            context,
            request,
            warnings: Vec::new(),
        }
    }

    fn probe(&self, step: Step) -> Option<Injected> {
        (self.context.probe)(step)
    }

    fn crash_at(&self, step: Step) -> Result<(), Crashed> {
        match self.probe(step) {
            Some(Injected::Crash) => Err(Crashed),
            _ => Ok(()),
        }
    }

    fn log(&self, step: Step, phase: Phase, detail: Option<String>) -> anyhow::Result<()> {
        if self.probe(step) == Some(Injected::JournalError) {
            anyhow::bail!("injected journal failure");
        }
        self.context.journal.append(&Entry {
            op: self.op.clone(),
            at: now(),
            key: self.request.key.clone(),
            path: self.request.path.clone(),
            phase,
            detail,
        })
    }

    /// Journal writes after the irreversible step only warn.
    fn log_after(&mut self, step: Step, phase: Phase) {
        if let Err(error) = self.log(step, phase, None) {
            self.warnings.push(format!("journal: {error:#}"));
        }
    }

    fn run(mut self) -> Option<Outcome> {
        self.steps().ok()
    }

    fn steps(&mut self) -> Result<Outcome, Crashed> {
        let path = self.request.path.to_path_buf();
        let quarantine = match self.request.dir {
            DirState::Present { dev, ino } => Some(Quarantine {
                path: YardPath::new(quarantine_path(&path, &self.op)),
                dev,
                ino,
            }),
            _ => None,
        };

        self.crash_at(Step::Intent)?;
        if let Err(error) = self.log(
            Step::Intent,
            Phase::Intent {
                quarantine: quarantine.clone(),
            },
            None,
        ) {
            return Ok(Outcome::Blocked {
                reason: format!("cannot write journal: {error:#}"),
            });
        }

        if let Some(quarantine) = &quarantine {
            self.crash_at(Step::Rename)?;
            if let Err(error) = rename_noreplace(&path, &quarantine.path) {
                self.close(Phase::RolledBack, Some(error.to_string()));
                return Ok(Outcome::Blocked {
                    reason: rename_failure(&error),
                });
            }
            let moved = dir_state(&quarantine.path);
            if moved
                != (DirState::Present {
                    dev: quarantine.dev,
                    ino: quarantine.ino,
                })
            {
                return Ok(self.roll_back(quarantine, "directory was replaced before removal"));
            }

            self.crash_at(Step::Quarantined)?;
            if let Err(error) = self.log(Step::Quarantined, Phase::Quarantined, None) {
                return Ok(self.roll_back(quarantine, &format!("cannot write journal: {error:#}")));
            }

            self.crash_at(Step::Scan)?;
            let watermark = i128::from(self.request.watermark);
            if let Err(refusal) = tree_scan::scan(&quarantine.path, watermark) {
                return Ok(self.roll_back(quarantine, &refusal.to_string()));
            }
        } else if dir_state(&path) != self.request.dir {
            self.close(Phase::RolledBack, None);
            return Ok(Outcome::Blocked {
                reason: "directory reappeared before removal".into(),
            });
        }

        self.crash_at(Step::GitRemove)?;
        if let Err(reason) = self.git_remove(&path) {
            return Ok(match &quarantine {
                Some(quarantine) => self.roll_back(quarantine, &reason),
                None => {
                    self.close(Phase::RolledBack, Some(reason.clone()));
                    Outcome::Blocked { reason }
                }
            });
        }

        // The registration is gone: from here on nothing is undone.
        self.crash_at(Step::RegistrationRemoved)?;
        self.log_after(Step::RegistrationRemoved, Phase::RegistrationRemoved);

        if let Some(quarantine) = &quarantine {
            self.crash_at(Step::Delete)?;
            if let Err(error) = std::fs::remove_dir_all(&quarantine.path) {
                let reason = format!("deleting files: {error}");
                self.close(Phase::Failed, Some(reason.clone()));
                return Ok(Outcome::Failed {
                    reason,
                    quarantine: Some(quarantine.path.clone()),
                });
            }
            self.crash_at(Step::Deleted)?;
            self.log_after(Step::Deleted, Phase::Deleted);
        }
        self.crash_at(Step::Done)?;
        self.log_after(Step::Done, Phase::Done);
        Ok(Outcome::Removed {
            branch: self.request.branch.clone(),
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    /// `git worktree remove` without `--force`: it refuses locked or dirty
    /// worktrees and anything that reappeared at the path. A failure is
    /// checked against the registration itself, never assumed.
    fn git_remove(&self, path: &Path) -> Result<(), String> {
        let mut command = cli::git(self.request.key.common_dir.as_path());
        command.args(["worktree", "remove", "--"]).arg(path);
        let failure = match cli::run(command) {
            Ok(_) => None,
            Err(error) => Some(format!("{error:#}")),
        };
        match registration_exists(&self.request.key) {
            Some(false) => Ok(()),
            Some(true) => Err(failure
                .unwrap_or_else(|| "git reported success but the registration remains".into())),
            None => Err(format!(
                "cannot tell whether the registration remains{}",
                failure.map_or_else(String::new, |failure| format!(" ({failure})"))
            )),
        }
    }

    /// Moves the quarantine back. The original path must still be free.
    fn roll_back(&mut self, quarantine: &Quarantine, reason: &str) -> Outcome {
        match rename_noreplace(&quarantine.path, &self.request.path) {
            Ok(()) => {
                self.close(Phase::RolledBack, Some(reason.to_owned()));
                Outcome::Blocked {
                    reason: reason.to_owned(),
                }
            }
            Err(error) => {
                let reason = format!("{reason}; restoring failed: {error}");
                self.close(Phase::Failed, Some(reason.clone()));
                Outcome::Failed {
                    reason,
                    quarantine: Some(quarantine.path.clone()),
                }
            }
        }
    }

    fn close(&mut self, phase: Phase, detail: Option<String>) {
        let step = match phase {
            Phase::Failed => Step::Delete,
            _ => Step::Done,
        };
        if let Err(error) = self.log(step, phase, detail) {
            self.warnings.push(format!("journal: {error:#}"));
        }
    }
}

/// `.<name>.git-yard-trash-<op>` beside the worktree: same directory, same
/// filesystem, so the rename is atomic.
pub fn quarantine_path(path: &Path, op: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.git-yard-trash-{op}"))
}

fn rename_failure(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EBUSY) | Some(libc::EXDEV) => "worktree directory is a mount point".into(),
        Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            "filesystem cannot rename without replacing; left untouched".into()
        }
        _ => format!("moving worktree aside: {error}"),
    }
}

/// `path_absent` re-exported for recovery.
pub fn is_free(path: &Path) -> bool {
    path_absent(path) == Some(true)
}
