//! Background fetches: supervised, tagged with their repository so a caller
//! can stop the fetch of a repository it is about to change.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::core::ids::RepoKey;

use super::cli;
use super::process::Supervisor;

const TIMEOUT: Duration = Duration::from_secs(120);

/// Fetches every remote of `repo`, running git in `dir`.
pub fn fetch(supervisor: &Supervisor, dir: &Path, repo: &RepoKey) -> Result<()> {
    cli::ensure_version()?;
    let mut command = cli::git(dir);
    command.args(["fetch", "--all", "--prune", "--quiet"]);
    supervisor.run(command, dir, TIMEOUT, Some(repo))?;
    Ok(())
}

/// `FETCH_HEAD` is rewritten by every fetch.
pub fn fetched_within(common_dir: &Path, window: Duration) -> bool {
    std::fs::metadata(common_dir.join("FETCH_HEAD"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < window)
}
