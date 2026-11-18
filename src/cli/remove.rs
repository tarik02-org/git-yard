//! `git-yard remove <PATH>...`: assess now, then remove.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Serialize;

use crate::core::candidate::{Candidate, Kind, Seed};
use crate::core::ids::{YardPath, now};
use crate::core::obs::Obs;
use crate::core::policy::eligibility;
use crate::fs::paths::canonicalize_allow_missing_leaf;
use crate::fs::walk::{FoundRepo, normalize_roots, read_gitdir_file, walk};
use crate::git::assess::observe;
use crate::git::inventory::inventory;
use crate::removal::protocol::{Context, no_probe, remove};
use crate::removal::reconcile::reconcile;
use crate::removal::request::{Outcome, Request};
use crate::scan::engine::Services;

use super::Session;
use super::exit::{self, Signals};

#[derive(Serialize)]
struct Removed {
    path: YardPath,
    #[serde(flatten)]
    outcome: Outcome,
}

pub(crate) fn run(
    session: &Session,
    paths: &[PathBuf],
    allow_dirty: bool,
    json: bool,
) -> Result<ExitCode> {
    let services = session.services()?;
    let signals = Signals::install()?;
    let (recoveries, _) = reconcile(&services.journal, &services.locks)?;
    for recovery in &recoveries {
        eprintln!("git-yard: {}", recovery.describe());
    }

    let mut results = Vec::new();
    for path in paths {
        let target = canonicalize_allow_missing_leaf(&session.cwd.join(path))
            .with_context(|| format!("resolving {}", path.display()))?;
        let outcome = if signals.received().is_some() {
            Outcome::Cancelled
        } else {
            match prepare(session, &services, &target, allow_dirty) {
                Ok(request) => {
                    let cancelled = || signals.received().is_some();
                    let context = Context {
                        registry: &services.registry,
                        journal: &services.journal,
                        locks: &services.locks,
                        cancelled: &cancelled,
                        on_removing: &|| {},
                        probe: &no_probe,
                    };
                    remove(&context, &request).expect("no crash probe is installed")
                }
                Err(reason) => Outcome::Blocked {
                    reason: format!("{reason:#}"),
                },
            }
        };
        if !json {
            println!("{}: {outcome}", crate::core::format::path(&target));
        }
        results.push(Removed {
            path: YardPath::new(target),
            outcome,
        });
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    let all_done = results
        .iter()
        .all(|result| result.outcome.is_clean_success());
    Ok(signals.finish(exit::code(if all_done { exit::OK } else { exit::PARTIAL })))
}

/// Finds the registration at `target`, assesses it now and turns it into a
/// request. Dirty worktrees need `allow_dirty`.
fn prepare(
    session: &Session,
    services: &Services,
    target: &Path,
    allow_dirty: bool,
) -> Result<Request> {
    let (repo, seed) = find_registration(session, services, target)?;
    let (facts, watermark) = observe(&repo, &seed, Arc::new(AtomicBool::new(false)))?
        .ok_or_else(|| anyhow!("assessment interrupted"))?;
    let dirty = facts.status.is_some_and(|status| !status.is_clean());
    let candidate = Candidate {
        seed,
        git: Obs::Complete {
            value: facts,
            observed_at: now(),
        },
        storage: Obs::Pending,
        watermark: Some(watermark),
    };
    if let Err(block) = eligibility(&candidate) {
        bail!("{block}");
    }
    if dirty && !allow_dirty {
        bail!("worktree has uncommitted or untracked changes; pass --allow-dirty to remove it");
    }
    Request::from_candidate(&candidate, 0).context("not a removable linked worktree")
}

/// The one registration whose path is `target`. An existing checkout names
/// its repository through `.git`; a missing one is looked up in every
/// repository under the scan roots.
fn find_registration(
    session: &Session,
    services: &Services,
    target: &Path,
) -> Result<(gix::Repository, Seed)> {
    let mut repos = Vec::new();
    if let Some(admin) = read_gitdir_file(&target.join(".git"))
        && let Ok(common) = std::fs::read(admin.join("commondir"))
    {
        let relative = gix::path::from_byte_slice(common.trim_ascii())?;
        repos.push(std::fs::canonicalize(admin.join(relative))?);
    } else {
        let roots = normalize_roots(&session.config.roots);
        walk(
            &roots,
            session.config.max_depth,
            &session.config.skip_dirs,
            || true,
            |found| repos.push(found.common_dir),
        );
    }
    let mut matches = Vec::new();
    for common_dir in repos {
        let Ok(repo) = services.registry.open(&common_dir) else {
            continue;
        };
        let found = FoundRepo {
            root: common_dir.clone(),
            common_dir,
        };
        let Ok(inventory) = inventory(&repo, &found, &session.config.roots) else {
            continue;
        };
        for seed in inventory.seeds {
            if seed.path.as_path() == target {
                matches.push((repo.clone(), seed));
            }
        }
    }
    match matches.len() {
        0 => bail!("no worktree is registered at {}", target.display()),
        1 => {
            let (repo, seed) = matches.remove(0);
            if seed.kind != Kind::LinkedWorktree {
                bail!("{} is not a linked worktree", target.display());
            }
            Ok((repo, seed))
        }
        count => bail!(
            "{count} registrations point at {}; remove them with git",
            target.display()
        ),
    }
}
