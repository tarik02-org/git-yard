//! Revalidation and removal of one linked worktree.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use git2::{Oid, Repository, WorktreeLockStatus, WorktreePruneOptions};
use serde::Serialize;

use crate::assess;
use crate::discovery::{dir_state, ownership_conflict};
use crate::model::{Candidate, CandidateId, DirState, Dirty, Head, Kind, Registration, now};
use crate::store::{Journal, JournalEntry, Phase};

static NEXT_OP: AtomicU64 = AtomicU64::new(0);

/// What the user approved: removal is refused if any of it changed.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: CandidateId,
    pub path: PathBuf,
    pub gitdir: PathBuf,
    pub dir: DirState,
    pub head: Head,
    pub dirty: Option<Dirty>,
}

impl Request {
    /// Builds a request from a fully assessed linked worktree.
    pub fn from_candidate(candidate: &Candidate) -> Option<Request> {
        let git = candidate.git.complete()?;
        if candidate.seed.kind != Kind::LinkedWorktree {
            return None;
        }
        Some(Request {
            id: candidate.seed.id.clone(),
            path: candidate.seed.path.clone(),
            gitdir: candidate.seed.gitdir.clone(),
            dir: candidate.seed.dir,
            head: git.head.clone(),
            dirty: git.dirty,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "result", content = "detail", rename_all = "kebab-case")]
pub enum Step {
    Done,
    AlreadyAbsent,
    Retained,
    NotAttempted,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub directory: Step,
    pub registration: Step,
    pub branch: Step,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Removed { report: Report },
    Cancelled,
    Blocked { reason: String },
    Failed { reason: String, report: Report },
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Done => write!(f, "done"),
            Step::AlreadyAbsent => write!(f, "already absent"),
            Step::Retained => write!(f, "retained"),
            Step::NotAttempted => write!(f, "not attempted"),
            Step::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Removed { report } => write!(
                f,
                "removed (directory {}, registration {}, branch {})",
                report.directory, report.registration, report.branch
            ),
            Outcome::Cancelled => write!(f, "cancelled"),
            Outcome::Blocked { reason } => write!(f, "blocked: {reason}"),
            Outcome::Failed { reason, report } => write!(
                f,
                "failed: {reason} (directory {}, registration {})",
                report.directory, report.registration
            ),
        }
    }
}

/// Revalidates the request against the repository and filesystem, then
/// removes the directory and prunes the registration. `cancelled` is checked
/// once more before destructive work; `on_removing` fires when it starts.
pub fn run(
    request: &Request,
    journal: &Journal,
    cancelled: impl Fn() -> bool,
    on_removing: impl FnOnce(),
) -> Outcome {
    let name = match &request.id.registration {
        Registration::Linked(name) => name.clone(),
        Registration::Main => {
            return Outcome::Blocked {
                reason: "main worktrees are never removed".into(),
            };
        }
    };
    let repo = match Repository::open(&request.id.repo) {
        Ok(repo) => repo,
        Err(error) => {
            return Outcome::Blocked {
                reason: format!("opening repository: {error}"),
            };
        }
    };

    let validated = match validate(&repo, &name, request) {
        Ok(validated) => validated,
        Err(Validation::Blocked(reason)) => return Outcome::Blocked { reason },
        Err(Validation::AlreadyGone) => {
            return Outcome::Removed {
                report: Report {
                    directory: Step::AlreadyAbsent,
                    registration: Step::AlreadyAbsent,
                    branch: retained(&request.head),
                },
            };
        }
    };
    if cancelled() {
        return Outcome::Cancelled;
    }

    on_removing();
    let op = format!(
        "{}-{}-{}",
        now(),
        std::process::id(),
        NEXT_OP.fetch_add(1, Ordering::Relaxed)
    );
    let log = |phase: Phase, detail: Option<String>| {
        journal.append(&JournalEntry {
            op: op.clone(),
            at: now(),
            id: request.id.clone(),
            path: request.path.clone(),
            gitdir: request.gitdir.clone(),
            phase,
            detail,
        })
    };
    if let Err(error) = log(Phase::Intent, None) {
        return Outcome::Blocked {
            reason: format!("cannot write journal: {error:#}"),
        };
    }

    let mut report = Report {
        directory: Step::NotAttempted,
        registration: Step::NotAttempted,
        branch: retained(&request.head),
    };

    report.directory = if validated.directory_present {
        match fs::remove_dir_all(&request.path) {
            Ok(()) => Step::Done,
            Err(error) => Step::Failed(error.to_string()),
        }
    } else {
        Step::AlreadyAbsent
    };
    if let Step::Failed(reason) = &report.directory {
        let reason = format!("removing directory: {reason}");
        let _ = log(Phase::Failed, Some(reason.clone()));
        return Outcome::Failed { reason, report };
    }
    let _ = log(Phase::DirectoryRemoved, None);

    report.registration = match prune(&repo, &name) {
        Ok(()) => Step::Done,
        Err(error) => Step::Failed(format!("{error:#}")),
    };
    if let Step::Failed(reason) = &report.registration {
        let reason = format!("pruning registration: {reason}");
        let _ = log(Phase::Failed, Some(reason.clone()));
        return Outcome::Failed { reason, report };
    }
    let _ = log(Phase::RegistrationPruned, None);
    let _ = log(Phase::Done, None);
    Outcome::Removed { report }
}

fn retained(head: &Head) -> Step {
    match head.branch() {
        Some(_) => Step::Retained,
        None => Step::NotAttempted,
    }
}

enum Validation {
    Blocked(String),
    AlreadyGone,
}

struct Validated {
    directory_present: bool,
}

fn validate(repo: &Repository, name: &str, request: &Request) -> Result<Validated, Validation> {
    let blocked = |reason: String| Validation::Blocked(reason);
    let current_dir = dir_state(&request.path);

    let worktree = match repo.find_worktree(name) {
        Ok(worktree) => worktree,
        Err(_) if current_dir == DirState::Missing => return Err(Validation::AlreadyGone),
        Err(_) => {
            return Err(blocked(
                "registration disappeared but the directory remains; left untouched".into(),
            ));
        }
    };
    if worktree.path() != request.path {
        return Err(blocked(format!(
            "registration now points to {}",
            worktree.path().display()
        )));
    }
    if let Ok(WorktreeLockStatus::Locked(reason)) = worktree.is_locked() {
        return Err(blocked(match reason {
            Some(reason) => format!("locked: {reason}"),
            None => "locked".into(),
        }));
    }

    let directory_present = match (request.dir, current_dir) {
        (DirState::Present { .. }, DirState::Present { .. }) if request.dir != current_dir => {
            return Err(blocked("directory was replaced since assessment".into()));
        }
        (DirState::Missing, DirState::Present { .. }) => {
            return Err(blocked("directory reappeared since assessment".into()));
        }
        (_, DirState::Present { .. }) => true,
        (_, DirState::Missing) => false,
    };

    if directory_present {
        if let Some(conflict) = ownership_conflict(&request.path, &request.gitdir) {
            return Err(blocked(conflict));
        }
        if contains(&request.path, &request.id.repo) {
            return Err(blocked("directory contains the repository storage".into()));
        }
    }

    let head = assess::read_head(repo, &request.gitdir, Kind::LinkedWorktree)
        .map_err(|error| blocked(format!("reading HEAD: {error:#}")))?;
    if head != request.head {
        return Err(blocked("HEAD changed since assessment".into()));
    }
    if let Head::Detached { oid } = &head {
        let oid = Oid::from_str(oid).map_err(|error| blocked(error.to_string()))?;
        match assess::has_unreachable_commits(repo, oid) {
            Ok(false) => {}
            Ok(true) => return Err(blocked("detached HEAD has unreachable commits".into())),
            Err(error) => return Err(blocked(format!("checking reachability: {error:#}"))),
        }
    }

    if directory_present {
        let dirty = assess::dirty(&request.path)
            .map_err(|error| blocked(format!("checking status: {error:#}")))?;
        if Some(dirty) != request.dirty {
            return Err(blocked("working tree changed since assessment".into()));
        }
    }

    Ok(Validated { directory_present })
}

fn contains(path: &Path, inner: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    inner.starts_with(path)
}

fn prune(repo: &Repository, name: &str) -> Result<()> {
    let worktree = repo
        .find_worktree(name)
        .with_context(|| format!("finding registration {name}"))?;
    // The directory is already gone; libgit2 only removes the registration.
    let mut options = WorktreePruneOptions::new();
    options.valid(true).working_tree(false).locked(false);
    worktree.prune(Some(&mut options))?;
    Ok(())
}
