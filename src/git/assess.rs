//! Git facts of one registration, read in-process.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use gix::ObjectId;

use crate::core::candidate::{Kind, Seed};
use crate::core::facts::{BaseRef, DirState, GitFacts, Head, Integration};
use crate::core::ids::WorktreeKey;
use crate::git::{graph, head, inventory, refs, status};

/// Commits that keep work alive once `excluding` is removed: every shared
/// ref, every other worktree's HEAD and its private refs.
pub fn reachability_roots(
    repo: &gix::Repository,
    excluding: &WorktreeKey,
) -> Result<Vec<ObjectId>> {
    let mut roots = refs::all_ref_commits(repo, None)?;
    let main = WorktreeKey::main(&excluding.repo());
    if &main != excluding
        && let Ok(id) = repo.head_id()
    {
        roots.push(id.detach());
    }
    for proxy in repo.worktrees()? {
        let id = proxy.id()?.to_owned();
        if excluding.admin_id.as_ref().is_some_and(|own| {
            gix::path::os_str_into_bstr(own.as_os_str()).ok() == Some(id.as_ref())
        }) {
            continue;
        }
        let worktree = proxy.into_repo_with_possibly_inaccessible_worktree()?;
        if let Ok(id) = worktree.head_id() {
            roots.push(id.detach());
        }
        for prefix in ["refs/worktree/", "refs/bisect/"] {
            roots.extend(refs::all_ref_commits(&worktree, Some(prefix))?);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Assesses one registration. `Ok(None)` when interrupted.
pub fn assess(
    repo: &gix::Repository,
    seed: &Seed,
    bases: &[BaseRef],
    interrupt: Arc<AtomicBool>,
) -> Result<Option<GitFacts>> {
    let worktree = inventory::worktree_repo(repo, seed)?;
    let head = head::read_head(&worktree)?;
    let tip = head
        .oid()
        .map(|oid| ObjectId::from_hex(oid.as_bytes()))
        .transpose()?;
    let commit = tip.map(|tip| repo.find_commit(tip)).transpose()?;
    let last_commit = commit
        .as_ref()
        .map(|commit| commit.time().map(|time| time.seconds))
        .transpose()?;
    let subject = commit.as_ref().and_then(|commit| {
        let message = commit.message().ok()?;
        Some(message.summary().to_string())
    });
    let integration = match tip {
        None => Integration::Unborn,
        Some(tip) => graph::integration(repo, head.branch(), tip, bases)?,
    };
    let linked = seed.kind == Kind::LinkedWorktree;
    let unreachable_commits = match (&head, tip) {
        (Head::Detached { .. }, Some(tip)) if linked => {
            let roots = reachability_roots(repo, &seed.key)?;
            graph::has_unreachable(repo, tip, &roots)?
        }
        _ => false,
    };
    let status = match seed.dir {
        DirState::Present { .. } if linked => {
            match status::worktree_status(&worktree, interrupt)? {
                Some(evidence) => Some(evidence),
                None => return Ok(None),
            }
        }
        DirState::Missing {
            parent_present: true,
        } if linked => Some(status::staged_status(&worktree)?),
        _ => None,
    };
    Ok(Some(GitFacts {
        head,
        last_commit,
        subject,
        integration,
        status,
        unreachable_commits,
        in_progress: head::in_progress(&worktree),
        private_modules: linked && seed.admin_dir.join("modules").exists(),
        bases: bases.to_vec(),
    }))
}

/// Nanoseconds since the epoch, the unit of assessment watermarks.
pub fn now_nanos() -> i64 {
    let nanos = crate::fs::tree_scan::system_time_nanos(std::time::SystemTime::now());
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

/// Assesses `seed` against the repository's current base refs and returns
/// the facts with the watermark taken before anything was read.
pub fn observe(
    repo: &gix::Repository,
    seed: &Seed,
    interrupt: Arc<AtomicBool>,
) -> Result<Option<(GitFacts, i64)>> {
    let watermark = now_nanos();
    let bases = refs::base_refs(repo);
    Ok(assess(repo, seed, &bases, interrupt)?.map(|facts| (facts, watermark)))
}
