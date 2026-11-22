//! Opening repositories: one thread-safe handle per common directory,
//! turned into a thread-local handle per job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail};

/// How repositories are opened. Tests use isolated options so the
/// developer's Git configuration cannot change results.
#[derive(Clone, Default)]
pub struct GitContext {
    options: gix::open::Options,
}

impl GitContext {
    /// No global or system configuration, no environment overrides.
    pub fn isolated() -> Self {
        Self {
            options: gix::open::Options::isolated(),
        }
    }

    pub fn open(&self, git_dir: &Path) -> Result<gix::ThreadSafeRepository> {
        let repo = self
            .options
            .clone()
            .open_path_as_is(true)
            .open(git_dir)
            .with_context(|| format!("opening {}", git_dir.display()))?;
        ensure_supported(&repo.to_thread_local())?;
        Ok(repo)
    }
}

/// Rejects repositories whose ref storage gix cannot read correctly.
fn ensure_supported(repo: &gix::Repository) -> Result<()> {
    let config = repo.config_snapshot();
    if let Some(storage) = config.string("extensions.refStorage")
        && storage.as_slice() != b"files"
    {
        bail!(
            "unsupported ref storage {:?} in {}",
            storage,
            repo.common_dir().display()
        );
    }
    if repo.common_dir().join("reftable").is_dir() {
        bail!(
            "unsupported reftable ref storage in {}",
            repo.common_dir().display()
        );
    }
    Ok(())
}

/// Repositories opened in this session, by canonical common directory.
#[derive(Default)]
pub struct Registry {
    context: GitContext,
    repos: Mutex<HashMap<PathBuf, gix::ThreadSafeRepository>>,
}

impl Registry {
    pub fn new(context: GitContext) -> Self {
        Self {
            context,
            repos: Mutex::default(),
        }
    }

    pub fn context(&self) -> &GitContext {
        &self.context
    }

    /// A thread-local handle on the repository at `common_dir`.
    pub fn open(&self, common_dir: &Path) -> Result<gix::Repository> {
        if let Some(repo) = self
            .repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(common_dir)
        {
            return Ok(repo.to_thread_local());
        }
        let repo = self.context.open(common_dir)?;
        let local = repo.to_thread_local();
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(common_dir.to_path_buf(), repo);
        Ok(local)
    }

    /// Drops the cached handle so the next open sees a fresh state.
    pub fn forget(&self, common_dir: &Path) {
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(common_dir);
    }
}
