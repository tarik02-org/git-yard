use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch.
pub type Timestamp = i64;

pub fn now() -> Timestamp {
    to_timestamp(SystemTime::now())
}

pub fn to_timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as Timestamp,
        Err(error) => -(error.duration().as_secs() as Timestamp),
    }
}

/// A candidate is identified by its owning repository (canonical common Git
/// directory) and its worktree registration, never by path or branch alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct CandidateId {
    pub repo: PathBuf,
    pub registration: Registration,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Registration {
    Main,
    Linked(String),
}

const MAIN_MARKER: &str = "@main";

impl fmt::Display for CandidateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match &self.registration {
            Registration::Main => MAIN_MARKER,
            Registration::Linked(name) => name,
        };
        write!(f, "{}::{}", self.repo.display(), name)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid candidate id {0:?}, expected <git-common-dir>::<worktree-name>")]
pub struct InvalidCandidateId(String);

impl FromStr for CandidateId {
    type Err = InvalidCandidateId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((repo, name)) = value.rsplit_once("::") else {
            return Err(InvalidCandidateId(value.to_owned()));
        };
        if repo.is_empty() || name.is_empty() {
            return Err(InvalidCandidateId(value.to_owned()));
        }
        let registration = if name == MAIN_MARKER {
            Registration::Main
        } else {
            Registration::Linked(name.to_owned())
        };
        Ok(Self {
            repo: PathBuf::from(repo),
            registration,
        })
    }
}

impl From<CandidateId> for String {
    fn from(id: CandidateId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for CandidateId {
    type Error = InvalidCandidateId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Repository-level kinds exist so repository cleanup can share the model
/// later; v1 only removes linked worktrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    LinkedWorktree,
    MainWorktree,
    BareRepository,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum DirState {
    Present { dev: u64, ino: u64 },
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseRef {
    /// Short name such as `main` or `origin/main`.
    pub name: String,
    pub oid: String,
    /// Local branch name this base corresponds to, used to recognise
    /// worktrees that have the base branch itself checked out.
    pub branch: String,
}

/// Cheap facts collected during discovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Seed {
    pub id: CandidateId,
    pub kind: Kind,
    pub path: PathBuf,
    pub repo_label: String,
    /// Administrative directory of the registration (`<common>/worktrees/<name>`
    /// for linked worktrees, the common directory otherwise).
    pub gitdir: PathBuf,
    pub locked: Option<Lock>,
    pub dir: DirState,
    /// Newest mtime of the registration's HEAD, index and HEAD reflog.
    pub git_activity: Option<Timestamp>,
    pub conflict: Option<String>,
    pub base: Vec<BaseRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Integration {
    /// The worktree has a base branch checked out.
    IsBase,
    /// The tip is an ancestor of the base.
    Merged {
        into: String,
    },
    /// Every path the branch changed since the merge base has the same
    /// content in the base (squash or rebase merge). Ancestry does not hold.
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dirty {
    pub changed: usize,
    pub untracked: usize,
    pub conflicted: usize,
    /// Evidence for the paths and contents approved for removal.
    #[serde(default)]
    pub fingerprint: [u8; 20],
}

impl Dirty {
    pub fn is_clean(&self) -> bool {
        self.changed == 0 && self.untracked == 0 && self.conflicted == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFacts {
    pub head: Head,
    pub last_commit: Option<Timestamp>,
    /// First line of the HEAD commit message.
    #[serde(default)]
    pub subject: Option<String>,
    pub integration: Integration,
    /// `None` when there is no working directory or it was not inspected.
    pub dirty: Option<Dirty>,
    /// Detached HEAD with commits that no reference reaches.
    pub unreachable_commits: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub logical: u64,
    pub allocated: u64,
    /// Bytes in files with more than one hard link; removing this worktree
    /// does not necessarily free them.
    pub hardlinked: u64,
    pub entries: u64,
    pub newest_mtime: Option<Timestamp>,
    pub skipped_mounts: u64,
    pub errors: u64,
}

/// An observation and how much it can be trusted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Obs<T> {
    Pending,
    /// Loaded from cache; never authorizes deletion.
    Stale {
        value: T,
        observed_at: Timestamp,
    },
    /// Still being collected.
    Partial {
        value: T,
    },
    Complete {
        value: T,
        observed_at: Timestamp,
    },
    Failed {
        error: String,
    },
}

impl<T> Obs<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Obs::Stale { value, .. } | Obs::Partial { value } | Obs::Complete { value, .. } => {
                Some(value)
            }
            Obs::Pending | Obs::Failed { .. } => None,
        }
    }

    pub fn complete(&self) -> Option<&T> {
        match self {
            Obs::Complete { value, .. } => Some(value),
            _ => None,
        }
    }

    pub fn is_settled(&self) -> bool {
        matches!(self, Obs::Complete { .. } | Obs::Failed { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub seed: Seed,
    pub git: Obs<GitFacts>,
    pub usage: Obs<Usage>,
}

impl Candidate {
    pub fn new(seed: Seed) -> Self {
        Self {
            seed,
            git: Obs::Pending,
            usage: Obs::Pending,
        }
    }

    pub fn id(&self) -> &CandidateId {
        &self.seed.id
    }

    /// Last observed use. Working-file modifications are the use signal;
    /// Git administrative activity (checkout, commit, index writes) counts too.
    pub fn last_use(&self) -> Option<Timestamp> {
        let modified = self.usage.value().and_then(|usage| usage.newest_mtime);
        match (modified, self.seed.git_activity) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether `last_use` already includes a full modification scan.
    pub fn last_use_complete(&self) -> bool {
        matches!(self.usage, Obs::Complete { .. })
    }

    pub fn head_label(&self) -> String {
        match self.git.value().map(|git| &git.head) {
            Some(Head::Branch { name, .. }) | Some(Head::Unborn { name }) => name.clone(),
            Some(Head::Detached { oid }) => format!("({})", &oid[..oid.len().min(8)]),
            None => String::new(),
        }
    }

    /// Allocated bytes where the filesystem reports them, logical otherwise.
    pub fn size(&self) -> Option<u64> {
        self.usage.value().map(|usage| {
            if usage.allocated > 0 {
                usage.allocated
            } else {
                usage.logical
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_id_round_trips() {
        let linked = CandidateId {
            repo: PathBuf::from("/home/u/work/a/b/.git"),
            registration: Registration::Linked("feature".into()),
        };
        let main = CandidateId {
            repo: PathBuf::from("/srv/repo.git"),
            registration: Registration::Main,
        };
        for id in [linked, main] {
            assert_eq!(id.to_string().parse::<CandidateId>().unwrap(), id);
        }
        assert!("no-separator".parse::<CandidateId>().is_err());
    }
}
