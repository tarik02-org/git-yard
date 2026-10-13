//! Where git-yard keeps its cache and state.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Locations {
    pub cache_file: PathBuf,
    pub pick_file: PathBuf,
    pub journal_file: PathBuf,
    /// Per-repository lock files.
    pub lock_dir: PathBuf,
}

impl Locations {
    /// The platform's cache and state directories, under `git-yard`.
    pub fn default_locations() -> Result<Self> {
        let cache = dirs::cache_dir().context("no cache directory")?;
        let state = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("no state directory")?;
        Ok(Self {
            cache_file: cache.join("git-yard/cache.json"),
            pick_file: cache.join("git-yard/pick.json"),
            journal_file: state.join("git-yard/journal.jsonl"),
            lock_dir: state.join("git-yard/locks"),
        })
    }

    pub fn under(dir: &Path) -> Self {
        Self {
            cache_file: dir.join("cache.json"),
            pick_file: dir.join("pick.json"),
            journal_file: dir.join("journal.jsonl"),
            lock_dir: dir.join("locks"),
        }
    }
}
