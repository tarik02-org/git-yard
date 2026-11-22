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
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    /// Directory and registration are gone; the branch is kept.
    Removed {
        branch: Option<String>,
    },
    /// Registration and directory had both disappeared already.
    AlreadyGone,
    Cancelled,
    /// Nothing was changed.
    Blocked {
        reason: String,
    },
    /// Git failed after deletion started; part of the directory may remain.
    Failed {
        reason: String,
    },
}

impl Outcome {
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
            Outcome::Removed { branch } => {
                write!(f, "removed")?;
                if let Some(branch) = branch {
                    write!(f, " (branch {branch} kept)")?;
                }
                Ok(())
            }
            Outcome::AlreadyGone => write!(f, "already gone"),
            Outcome::Cancelled => write!(f, "cancelled"),
            Outcome::Blocked { reason } => write!(f, "blocked: {reason}"),
            Outcome::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}
