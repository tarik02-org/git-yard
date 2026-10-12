//! Per-repository mutual exclusion for removals: an in-process lock, then an
//! advisory file lock so other git-yard processes wait too.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};

use crate::core::ids::RepoKey;

#[derive(Debug, Default)]
struct Held {
    repos: Mutex<HashSet<RepoKey>>,
    released: Condvar,
}

impl Held {
    fn repos(&self) -> MutexGuard<'_, HashSet<RepoKey>> {
        self.repos.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub struct RepoLocks {
    dir: PathBuf,
    held: Arc<Held>,
}

/// Held while a repository is being modified.
#[derive(Debug)]
pub struct RepoGuard {
    held: Arc<Held>,
    repo: RepoKey,
    _file: File,
}

impl Drop for RepoGuard {
    fn drop(&mut self) {
        self.held.repos().remove(&self.repo);
        self.held.released.notify_all();
    }
}

impl RepoLocks {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            held: Arc::default(),
        }
    }

    fn file(&self, repo: &RepoKey) -> Result<File> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(format!("{}.lock", lock_name(repo.path())));
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))
    }

    fn guard(&self, repo: &RepoKey, file: File) -> RepoGuard {
        RepoGuard {
            held: self.held.clone(),
            repo: repo.clone(),
            _file: file,
        }
    }

    /// Blocks until this process and every other one leave `repo` alone.
    pub fn lock(&self, repo: &RepoKey) -> Result<RepoGuard> {
        {
            let mut repos = self.held.repos();
            while repos.contains(repo) {
                repos = self
                    .held
                    .released
                    .wait(repos)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            repos.insert(repo.clone());
        }
        let release = |error| {
            self.held.repos().remove(repo);
            self.held.released.notify_all();
            error
        };
        let file = self.file(repo).map_err(release)?;
        file.lock().context("locking repository").map_err(release)?;
        Ok(self.guard(repo, file))
    }

    /// `None` when another thread or process holds the lock.
    pub fn try_lock(&self, repo: &RepoKey) -> Result<Option<RepoGuard>> {
        if !self.held.repos().insert(repo.clone()) {
            return Ok(None);
        }
        let release = || {
            self.held.repos().remove(repo);
            self.held.released.notify_all();
        };
        let file = match self.file(repo) {
            Ok(file) => file,
            Err(error) => {
                release();
                return Err(error);
            }
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(self.guard(repo, file))),
            Err(std::fs::TryLockError::WouldBlock) => {
                release();
                Ok(None)
            }
            Err(std::fs::TryLockError::Error(error)) => {
                release();
                Err(error).context("locking repository")
            }
        }
    }
}

/// A stable file name for a repository: a hash of its canonical common dir.
fn lock_name(common_dir: &Path) -> String {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(common_dir.as_os_str().as_bytes());
    let digest = hasher.try_finalize().expect("sha1 never fails");
    digest.to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_waits_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let locks = RepoLocks::new(dir.path());
        let repo = RepoKey::new("/r/.git");
        let guard = locks.lock(&repo).unwrap();
        assert!(locks.try_lock(&repo).unwrap().is_none());
        assert!(locks.try_lock(&RepoKey::new("/s/.git")).unwrap().is_some());
        drop(guard);
        assert!(locks.try_lock(&repo).unwrap().is_some());
    }
}
