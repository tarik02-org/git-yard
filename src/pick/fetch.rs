//! Background fetches: supervised, tagged with their repository so a switch
//! can stop the fetch of the repository it is about to change.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::git::cli;
use crate::git::process::Supervisor;

use super::listing::Project;

const TIMEOUT: Duration = Duration::from_secs(120);

pub fn fetch(supervisor: &Supervisor, project: &Project) -> Result<()> {
    cli::ensure_version()?;
    let mut command = cli::git(&project.main_path);
    command.args(["fetch", "--all", "--prune", "--quiet"]);
    supervisor.run(command, &project.main_path, TIMEOUT, Some(&project.repo))?;
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
