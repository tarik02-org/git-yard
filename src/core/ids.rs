//! Identities of repositories and worktree registrations, and a path type
//! that survives JSON round trips when it is not UTF-8.

use std::ffi::OsString;
use std::fmt;
use std::ops::Deref;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// A path serialized as `{"utf8": "…"}` when it is valid UTF-8, otherwise
/// as `{"bytes": "<base64>", "lossy": "…"}` so the exact bytes survive.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct YardPath(pub PathBuf);

impl YardPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl Deref for YardPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for YardPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<PathBuf> for YardPath {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}

impl From<&Path> for YardPath {
    fn from(path: &Path) -> Self {
        Self(path.to_path_buf())
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReprOut<'a> {
    Utf8 { utf8: &'a str },
    Bytes { bytes: String, lossy: String },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReprIn {
    Utf8 {
        utf8: String,
    },
    Bytes {
        bytes: String,
        #[allow(dead_code)]
        #[serde(default)]
        lossy: Option<String>,
    },
}

impl Serialize for YardPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.to_str() {
            Some(utf8) => ReprOut::Utf8 { utf8 }.serialize(serializer),
            None => ReprOut::Bytes {
                bytes: BASE64.encode(self.0.as_os_str().as_bytes()),
                lossy: self.0.to_string_lossy().into_owned(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for YardPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ReprIn::deserialize(deserializer)? {
            ReprIn::Utf8 { utf8 } => Ok(Self(PathBuf::from(utf8))),
            ReprIn::Bytes { bytes, .. } => {
                let bytes = BASE64.decode(bytes).map_err(serde::de::Error::custom)?;
                Ok(Self(PathBuf::from(OsString::from_vec(bytes))))
            }
        }
    }
}

/// A repository, identified by its canonical common Git directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoKey {
    pub common_dir: YardPath,
}

impl RepoKey {
    pub fn new(common_dir: impl Into<PathBuf>) -> Self {
        Self {
            common_dir: YardPath::new(common_dir),
        }
    }

    pub fn path(&self) -> &Path {
        &self.common_dir
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.common_dir.display())
    }
}

/// A worktree registration: the main worktree of a repository (`admin_id`
/// is `None`) or a linked worktree, named by its directory under
/// `<common>/worktrees/`. Never identified by path or branch alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorktreeKey {
    pub common_dir: YardPath,
    pub admin_id: Option<YardPath>,
}

impl WorktreeKey {
    pub fn main(repo: &RepoKey) -> Self {
        Self {
            common_dir: repo.common_dir.clone(),
            admin_id: None,
        }
    }

    pub fn linked(repo: &RepoKey, admin_id: impl Into<PathBuf>) -> Self {
        Self {
            common_dir: repo.common_dir.clone(),
            admin_id: Some(YardPath::new(admin_id)),
        }
    }

    pub fn repo(&self) -> RepoKey {
        RepoKey {
            common_dir: self.common_dir.clone(),
        }
    }

    pub fn is_main(&self) -> bool {
        self.admin_id.is_none()
    }
}

impl fmt::Display for WorktreeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.admin_id {
            Some(id) => write!(f, "{}::{}", self.common_dir.display(), id.display()),
            None => write!(f, "{}::@main", self.common_dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_utf8_paths_round_trip_through_json() {
        let raw = PathBuf::from(OsString::from_vec(b"/tmp/caf\xe9".to_vec()));
        for path in [PathBuf::from("/tmp/café"), raw] {
            let key = WorktreeKey {
                common_dir: YardPath::new(&path),
                admin_id: Some(YardPath::new(path.file_name().unwrap())),
            };
            let json = serde_json::to_string(&key).unwrap();
            assert_eq!(serde_json::from_str::<WorktreeKey>(&json).unwrap(), key);
        }
        let json =
            serde_json::to_value(YardPath::new(OsString::from_vec(b"a\xff".to_vec()))).unwrap();
        assert_eq!(json["bytes"], "Yf8=");
        assert_eq!(json["lossy"], "a\u{fffd}");
    }
}
