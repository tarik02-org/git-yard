//! Fresh facts compared with a request's evidence.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::core::candidate::{Candidate, Seed};
use crate::core::ids::WorktreeKey;
use crate::core::obs::Obs;
use crate::core::policy::eligibility;
use crate::fs::walk::FoundRepo;
use crate::git::assess;
use crate::git::inventory::inventory;
use crate::git::open::Registry;

use super::request::Request;

pub enum Validation {
    Blocked(String),
    /// Registration and directory are both definitely absent.
    AlreadyGone,
}

/// `<common>/worktrees/<id>` of a linked registration.
pub fn admin_dir(key: &WorktreeKey) -> Option<PathBuf> {
    let id = key.admin_id.as_ref()?;
    Some(key.common_dir.join("worktrees").join(id.as_path()))
}

/// `Some(true)` when the registration exists, `Some(false)` only when its
/// directory is definitely gone; `None` when that cannot be told.
pub fn registration_exists(key: &WorktreeKey) -> Option<bool> {
    let admin = admin_dir(key)?;
    match std::fs::symlink_metadata(&admin) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

pub fn path_absent(path: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Some(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(true),
        Err(_) => None,
    }
}

/// Re-reads the registration and its facts and checks that the decision
/// still holds. Returns the fresh candidate.
pub fn validate(registry: &Registry, request: &Request) -> Result<Candidate, Validation> {
    let blocked = |reason: String| Validation::Blocked(reason);
    let common = request.key.common_dir.to_path_buf();
    registry.forget(&common);
    let repo = registry
        .open(&common)
        .map_err(|error| blocked(format!("opening repository: {error:#}")))?;
    let found = FoundRepo {
        common_dir: common.clone(),
        root: common.clone(),
    };
    let inventory = inventory(&repo, &found, &[])
        .map_err(|error| blocked(format!("reading registrations: {error:#}")))?;
    let Some(seed) = inventory
        .seeds
        .into_iter()
        .find(|seed| seed.key == request.key)
    else {
        return match (
            registration_exists(&request.key),
            path_absent(&request.path),
        ) {
            (Some(false), Some(true)) => Err(Validation::AlreadyGone),
            _ => Err(blocked(
                "registration disappeared or is unreadable; left untouched".into(),
            )),
        };
    };
    let seed = Seed {
        // Labels depend on scan roots and are not evidence.
        repo_label: String::new(),
        ..seed
    };
    if seed.path != request.path {
        return Err(blocked(format!(
            "registration now points to {}",
            seed.path.display()
        )));
    }
    if seed.dir != request.dir {
        return Err(blocked("directory changed since assessment".into()));
    }
    let (facts, watermark) = assess::observe(&repo, &seed, Arc::new(AtomicBool::new(false)))
        .map_err(|error| blocked(format!("reassessing: {error:#}")))?
        .ok_or_else(|| blocked("reassessment interrupted".into()))?;
    let candidate = Candidate {
        seed,
        git: Obs::Complete {
            value: facts,
            observed_at: crate::core::ids::now(),
        },
        storage: Obs::Pending,
        watermark: Some(watermark),
    };
    eligibility(&candidate).map_err(|block| blocked(block.to_string()))?;
    if candidate.revision() != request.revision {
        return Err(blocked(describe_change(&candidate)));
    }
    Ok(candidate)
}

fn describe_change(fresh: &Candidate) -> String {
    let Some(git) = fresh.git.complete() else {
        return "facts changed since assessment".into();
    };
    format!(
        "changed since assessment (now {} at {}, {})",
        git.head.label(),
        git.head
            .oid()
            .map_or("no commit", |oid| &oid[..oid.len().min(8)]),
        git.status
            .map_or_else(|| "no status".to_owned(), |status| status.describe())
    )
}
