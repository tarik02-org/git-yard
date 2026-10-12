//! Recovery of removals interrupted by a crash or a kill. Only operations
//! whose repository nobody holds are touched; files are never deleted.

use serde::Serialize;

use crate::core::facts::DirState;
use crate::core::ids::{WorktreeKey, YardPath, now};
use crate::fs::dirstate::dir_state;
use crate::fs::rename::rename_noreplace;
use crate::store::journal::{Entry, Journal, Operation, Phase};
use crate::store::lock::RepoLocks;

use super::protocol::is_free;
use super::validate::registration_exists;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// The directory was moved back; the worktree is as before.
    Restored,
    /// Nothing is left to do.
    Resolved,
    /// Needs a person: `quarantine` holds files that were not put back.
    Attention {
        message: String,
        quarantine: Option<YardPath>,
    },
    /// Another git-yard process is working on this repository.
    Busy,
}

#[derive(Clone, Debug, Serialize)]
pub struct Recovery {
    pub op: String,
    pub key: WorktreeKey,
    pub path: YardPath,
    pub last_phase: &'static str,
    #[serde(flatten)]
    pub action: Action,
}

impl Recovery {
    pub fn describe(&self) -> String {
        let path = crate::core::format::path(&self.path);
        match &self.action {
            Action::Restored => format!("restored {path} after an interrupted removal"),
            Action::Resolved => format!("interrupted removal of {path} needed no recovery"),
            Action::Busy => format!("removal of {path} is in progress elsewhere"),
            Action::Attention {
                message,
                quarantine,
            } => match quarantine {
                Some(quarantine) => format!(
                    "interrupted removal of {path}: {message}; files at {}",
                    crate::core::format::path(quarantine)
                ),
                None => format!("interrupted removal of {path}: {message}"),
            },
        }
    }
}

/// How long recovery waits for a repository before reporting it busy.
const BUSY_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// Recovers what it safely can and reports every unfinished operation.
pub fn reconcile(journal: &Journal, locks: &RepoLocks) -> anyhow::Result<(Vec<Recovery>, usize)> {
    let (unfinished, parse_errors) = journal.unfinished()?;
    let mut recoveries = Vec::new();
    for operation in unfinished {
        let repo = operation.key().repo();
        let action = match locks.lock_within(&repo, BUSY_AFTER)? {
            None => Action::Busy,
            Some(_guard) => {
                let action = recover(&operation);
                let phase = match &action {
                    Action::Restored => Some(Phase::Restored),
                    Action::Resolved => Some(Phase::Resolved),
                    Action::Attention { .. } | Action::Busy => None,
                };
                if let Some(phase) = phase {
                    journal.append(&Entry {
                        op: operation.op().to_owned(),
                        at: now(),
                        key: operation.key().clone(),
                        path: operation.path().clone(),
                        phase,
                        detail: None,
                    })?;
                }
                action
            }
        };
        recoveries.push(Recovery {
            op: operation.op().to_owned(),
            key: operation.key().clone(),
            path: operation.path().clone(),
            last_phase: operation.last().phase.name(),
            action,
        });
    }
    Ok((recoveries, parse_errors))
}

fn recover(operation: &Operation) -> Action {
    let registered = registration_exists(operation.key());
    let original_free = is_free(operation.path());
    let Some(quarantine) = operation.quarantine() else {
        // The directory was already missing; the only change is the
        // registration, which is either still there or gone as approved.
        return match registered {
            Some(_) => Action::Resolved,
            None => attention("cannot tell whether the registration remains", None),
        };
    };
    let quarantined = dir_state(&quarantine.path);
    let ours = quarantined
        == DirState::Present {
            dev: quarantine.dev,
            ino: quarantine.ino,
        };
    let quarantine_gone = is_free(&quarantine.path);

    match (registered, ours, quarantine_gone) {
        // Crashed before the rename, or after a rollback.
        (Some(true), false, true) if !original_free => Action::Resolved,
        (Some(true), true, _) if original_free => {
            match rename_noreplace(&quarantine.path, operation.path()) {
                Ok(()) => Action::Restored,
                Err(error) => attention(
                    &format!("could not move files back: {error}"),
                    Some(quarantine.path.clone()),
                ),
            }
        }
        (Some(true), true, _) => attention(
            "something now occupies the worktree path",
            Some(quarantine.path.clone()),
        ),
        // The registration was removed as approved; the files are deleted
        // only on the user's say-so.
        (Some(false), true, _) => attention(
            "the registration was removed but its files were not deleted",
            Some(quarantine.path.clone()),
        ),
        (Some(false), false, true) => Action::Resolved,
        _ => attention(
            "state could not be determined",
            (!quarantine_gone).then(|| quarantine.path.clone()),
        ),
    }
}

fn attention(message: &str, quarantine: Option<YardPath>) -> Action {
    Action::Attention {
        message: message.to_owned(),
        quarantine,
    }
}
