//! What the user approved, and what came of it.

use std::fmt;

use serde::Serialize;

use crate::core::candidate::{Candidate, Kind};
use crate::core::facts::{DecisionRevision, DirState};
use crate::core::ids::{WorktreeKey, YardPath};

/// A removal request: the evidence a decision was made on. Removal refuses
/// to proceed if any of it changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Submission generation; workers drop requests of older generations.
    pub generation: u64,
    pub key: WorktreeKey,
    pub path: YardPath,
    pub dir: DirState,
    pub revision: DecisionRevision,
    pub branch: Option<String>,
    /// Assessment start in nanoseconds; later changes in the tree block.
    pub watermark: i64,
}

impl Request {
    /// A request from a fully assessed linked worktree, if it has one.
    pub fn from_candidate(candidate: &Candidate, generation: u64) -> Option<Request> {
        let git = candidate.git.complete()?;
        if candidate.seed.kind != Kind::LinkedWorktree {
            return None;
        }
        Some(Request {
            generation,
            key: candidate.seed.key.clone(),
            path: candidate.seed.path.clone(),
            dir: candidate.seed.dir,
            revision: candidate.revision(),
            branch: git.head.branch().map(str::to_owned),
            watermark: candidate.watermark?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// Directory and registration are gone; the branch is kept.
    Removed {
        branch: Option<String>,
        /// Problems after the irreversible step, such as journal writes.
        warnings: Vec<String>,
    },
    /// Registration and directory had both disappeared already.
    AlreadyGone,
    Cancelled,
    /// Nothing was changed.
    Blocked {
        reason: String,
    },
    /// Something went wrong after changes started; `quarantine` holds the
    /// directory if it could not be restored or deleted.
    Failed {
        reason: String,
        quarantine: Option<YardPath>,
    },
}

impl Outcome {
    /// Done, and nothing needs attention.
    pub fn is_clean_success(&self) -> bool {
        match self {
            Outcome::Removed { warnings, .. } => warnings.is_empty(),
            Outcome::AlreadyGone => true,
            _ => false,
        }
    }

    pub fn is_removed(&self) -> bool {
        matches!(self, Outcome::Removed { .. } | Outcome::AlreadyGone)
    }

    /// May be submitted again (after a rescan provides fresh evidence).
    pub fn allows_retry(&self) -> bool {
        matches!(self, Outcome::Cancelled | Outcome::Blocked { .. })
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Removed { branch, warnings } => {
                write!(f, "removed")?;
                if let Some(branch) = branch {
                    write!(f, " (branch {branch} kept)")?;
                }
                for warning in warnings {
                    write!(f, "; warning: {warning}")?;
                }
                Ok(())
            }
            Outcome::AlreadyGone => write!(f, "already gone"),
            Outcome::Cancelled => write!(f, "cancelled"),
            Outcome::Blocked { reason } => write!(f, "blocked: {reason}"),
            Outcome::Failed { reason, quarantine } => {
                write!(f, "failed: {reason}")?;
                if let Some(quarantine) = quarantine {
                    write!(f, "; files kept at {}", quarantine.display())?;
                }
                Ok(())
            }
        }
    }
}
