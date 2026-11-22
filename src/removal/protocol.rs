//! Removal of one linked worktree: revalidate under the repository lock,
//! then let `git worktree remove --force` delete it.
//!
//! Ignored files are disposable, as with `git clean -X`, so nothing below
//! the worktree is inspected beyond what status already reports, except
//! that a filesystem mounted inside it is never deleted through.

use std::path::Path;

use crate::fs::mounts::mount_below;
use crate::git::cli;
use crate::git::open::Registry;
use crate::store::lock::RepoLocks;

use super::request::{Outcome, Request};
use super::validate::{Validation, registration_exists, validate};

pub struct Context<'a> {
    pub registry: &'a Registry,
    pub locks: &'a RepoLocks,
    /// Checked before anything is changed.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Reports that validation passed and changes are starting.
    pub on_removing: &'a (dyn Fn() + Sync),
}

pub fn remove(context: &Context<'_>, request: &Request) -> Outcome {
    let repo = request.key.repo();
    let _guard = match context.locks.lock(&repo) {
        Ok(guard) => guard,
        Err(error) => {
            return Outcome::Blocked {
                reason: format!("{error:#}"),
            };
        }
    };
    if (context.cancelled)() {
        return Outcome::Cancelled;
    }
    match validate(context.registry, request) {
        Ok(_) => {}
        Err(Validation::Blocked(reason)) => return Outcome::Blocked { reason },
        Err(Validation::AlreadyGone) => return Outcome::AlreadyGone,
    }
    let canonical = request
        .path
        .canonicalize()
        .unwrap_or_else(|_| request.path.to_path_buf());
    match mount_below(&canonical) {
        Ok(None) => {}
        Ok(Some(mount)) => {
            return Outcome::Blocked {
                reason: format!("contains a mounted filesystem: {}", mount.display()),
            };
        }
        Err(error) => {
            return Outcome::Blocked {
                reason: format!("reading mount table: {error}"),
            };
        }
    }
    if (context.cancelled)() {
        return Outcome::Cancelled;
    }
    (context.on_removing)();
    match git_remove(request, &request.path) {
        Ok(()) => Outcome::Removed {
            branch: request.branch.clone(),
        },
        Err(reason) => Outcome::Failed { reason },
    }
}

/// `--force` lets Git delete dirty worktrees and ones with submodules; the
/// user approved exactly the state validation just confirmed. A single
/// `--force` still refuses locked worktrees. A failure is checked against
/// the registration itself, never assumed.
fn git_remove(request: &Request, path: &Path) -> Result<(), String> {
    let mut command = cli::git(request.key.common_dir.as_path());
    command
        .args(["worktree", "remove", "--force", "--"])
        .arg(path);
    let failure = match cli::run(command) {
        Ok(_) => None,
        Err(error) => Some(format!("{error:#}")),
    };
    match registration_exists(&request.key) {
        Some(false) => Ok(()),
        Some(true) => {
            Err(failure
                .unwrap_or_else(|| "git reported success but the registration remains".into()))
        }
        None => Err(format!(
            "cannot tell whether the registration remains{}",
            failure.map_or_else(String::new, |failure| format!(" ({failure})"))
        )),
    }
}
