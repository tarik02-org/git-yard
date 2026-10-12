//! One worktree registration with everything observed about it.

use serde::{Deserialize, Serialize};

use super::facts::{DecisionRevision, DirState, GitFacts, Lock, StorageFacts};
use super::ids::{Timestamp, WorktreeKey, YardPath};
use super::obs::Obs;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    LinkedWorktree,
    MainWorktree,
    BareRepository,
}

/// Cheap facts from the inventory: registration files and one `lstat`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    pub key: WorktreeKey,
    pub kind: Kind,
    pub path: YardPath,
    pub repo_label: String,
    /// `<common>/worktrees/<id>` for linked worktrees, the common directory
    /// otherwise.
    pub admin_dir: YardPath,
    pub lock: Option<Lock>,
    pub dir: DirState,
    /// Newest mtime of the registration's HEAD, index and HEAD reflog.
    pub git_activity: Option<Timestamp>,
    /// The registration could not be read; it is shown but never removed.
    pub registration_error: Option<String>,
    /// Another registration resolves to the same directory.
    pub conflict: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub seed: Seed,
    pub git: Obs<GitFacts>,
    pub storage: Obs<StorageFacts>,
    /// When the current git observation started, in nanoseconds: anything
    /// changed at or after it is not covered by the evidence.
    #[serde(default)]
    pub watermark: Option<i64>,
}

impl Candidate {
    pub fn new(seed: Seed) -> Self {
        Self {
            seed,
            git: Obs::Pending,
            storage: Obs::Pending,
            watermark: None,
        }
    }

    pub fn key(&self) -> &WorktreeKey {
        &self.seed.key
    }

    /// Changes whenever a fact the removal decision rests on changes.
    pub fn revision(&self) -> DecisionRevision {
        let seed = &self.seed;
        let mut revision = DecisionRevision::of(
            &seed.dir,
            seed.lock.as_ref(),
            &seed.path,
            self.git.complete(),
        );
        if seed.conflict.is_some() || seed.registration_error.is_some() {
            revision.0 = !revision.0;
        }
        revision
    }

    /// Last observed use: working-file modifications, or Git administrative
    /// activity (checkout, commit, index writes).
    pub fn last_use(&self) -> Option<Timestamp> {
        let modified = self.storage.value().and_then(|facts| facts.newest_mtime);
        match (modified, self.seed.git_activity) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn head_label(&self) -> String {
        self.git
            .value()
            .map(|git| git.head.label())
            .unwrap_or_default()
    }

    pub fn size(&self) -> Option<u64> {
        self.storage.value().map(StorageFacts::size)
    }
}
