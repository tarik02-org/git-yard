//! Facts observed about one worktree registration. Everything that can change
//! a removal decision is folded into a `DecisionRevision`.

use serde::{Deserialize, Serialize};

use super::ids::{Timestamp, YardPath};

/// What is at a registration's checkout path, observed without following
/// symlinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum DirState {
    Present {
        dev: u64,
        ino: u64,
    },
    /// Nothing at the path. `parent_present` tells an unmounted volume
    /// (parent missing too) from a deleted leaf.
    Missing {
        parent_present: bool,
    },
    /// Something other than a directory: a symlink, file or special file.
    NotADirectory {
        kind: EntryKind,
    },
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Symlink,
    File,
    Special,
}

impl DirState {
    pub fn is_present(&self) -> bool {
        matches!(self, DirState::Present { .. })
    }

    /// Only the leaf directory is gone; the registration may be pruned.
    pub fn is_missing_leaf(&self) -> bool {
        matches!(
            self,
            DirState::Missing {
                parent_present: true
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Head {
    Branch { name: String, oid: String },
    Unborn { name: String },
    Detached { oid: String },
}

impl Head {
    pub fn oid(&self) -> Option<&str> {
        match self {
            Head::Branch { oid, .. } | Head::Detached { oid } => Some(oid),
            Head::Unborn { .. } => None,
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch { name, .. } | Head::Unborn { name } => Some(name),
            Head::Detached { .. } => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Head::Branch { name, .. } | Head::Unborn { name } => name.clone(),
            Head::Detached { oid } => format!("({})", &oid[..oid.len().min(8)]),
        }
    }
}

/// A branch integration is measured against.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseRef {
    /// Short name such as `main` or `upstream/main`.
    pub name: String,
    pub oid: String,
    /// Local branch name this base corresponds to.
    pub branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Integration {
    /// The worktree has a base branch checked out.
    IsBase,
    /// The tip is an ancestor of the base.
    Merged {
        into: String,
    },
    /// Every path the branch changed since the merge base has the same
    /// content in the base (squash or rebase merge).
    ChangesPresent {
        into: String,
    },
    NotIntegrated {
        base: String,
        ahead: usize,
    },
    NoBase,
    Unborn,
}

impl Integration {
    pub fn is_integrated(&self) -> bool {
        matches!(
            self,
            Integration::Merged { .. } | Integration::ChangesPresent { .. }
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Integration::IsBase => "base branch".into(),
            Integration::Merged { into } => format!("merged into {into}"),
            Integration::ChangesPresent { into } => {
                format!("changes present in {into} (squash/rebase)")
            }
            Integration::NotIntegrated { base, ahead } => {
                format!("{ahead} commits not in {base}")
            }
            Integration::NoBase => "no base branch".into(),
            Integration::Unborn => "no commits".into(),
        }
    }
}

/// Working-tree status. Ignored files are counted but not hashed: they are
/// covered by the ctime watermark at removal time instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StatusEvidence {
    pub changed: usize,
    pub staged: usize,
    pub untracked: usize,
    pub conflicted: usize,
    pub ignored: usize,
    /// Hash of every non-ignored entry and its content.
    pub hash: [u8; 20],
}

impl StatusEvidence {
    pub fn is_clean(&self) -> bool {
        self.changed == 0 && self.staged == 0 && self.untracked == 0 && self.conflicted == 0
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (count, label) in [
            (self.changed, "changed"),
            (self.staged, "staged"),
            (self.untracked, "untracked"),
            (self.conflicted, "conflicted"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        if parts.is_empty() {
            "clean".into()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Lock {
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    ApplyMailbox,
}

impl InProgress {
    pub fn describe(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
            InProgress::Bisect => "bisect",
            InProgress::ApplyMailbox => "am",
        }
    }
}

/// Everything Git says about a registration, collected without walking the
/// working tree beyond what status needs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitFacts {
    pub head: Head,
    pub last_commit: Option<Timestamp>,
    pub subject: Option<String>,
    pub integration: Integration,
    /// `None` when there is no working directory or it was not inspected.
    pub status: Option<StatusEvidence>,
    /// Detached HEAD with commits no reference or other worktree reaches.
    pub unreachable_commits: bool,
    pub in_progress: Option<InProgress>,
    /// `<admin>/modules` exists: private submodule repositories that pruning
    /// the registration would destroy.
    pub private_modules: bool,
    /// The bases integration was measured against, for revalidation.
    pub bases: Vec<BaseRef>,
}

/// Size and storage facts from the measurement walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageFacts {
    pub logical: u64,
    pub allocated: u64,
    /// Bytes in files with more than one hard link; removing the worktree
    /// does not necessarily free them.
    pub hardlinked: u64,
    pub entries: u64,
    pub newest_mtime: Option<Timestamp>,
    /// Directories on another filesystem.
    pub mounts: u64,
    /// Nested `.git` entries or bare repositories below the root.
    pub nested_repos: u64,
    pub errors: u64,
}

impl StorageFacts {
    /// Allocated bytes where the filesystem reports them, logical otherwise.
    pub fn size(&self) -> u64 {
        if self.allocated > 0 {
            self.allocated
        } else {
            self.logical
        }
    }
}

/// Hash of every fact a removal decision rests on. Size is left out, so
/// measuring a row does not invalidate the user's choices about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecisionRevision(pub u64);

impl DecisionRevision {
    pub fn of(
        dir: &DirState,
        lock: Option<&Lock>,
        path: &YardPath,
        git: Option<&GitFacts>,
    ) -> Self {
        use std::hash::{Hash, Hasher};
        // SipHash with fixed keys: stable within and across runs of one build.
        #[allow(deprecated)]
        let mut hasher = std::hash::SipHasher::new();
        dir.hash(&mut hasher);
        lock.hash(&mut hasher);
        path.hash(&mut hasher);
        git.hash(&mut hasher);
        Self(hasher.finish())
    }
}
