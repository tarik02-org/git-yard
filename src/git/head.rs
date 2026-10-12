//! HEAD and in-progress operations of one worktree.

use anyhow::Result;
use gix::prelude::ReferenceExt as _;

use crate::core::facts::{Head, InProgress};

/// Reads HEAD of a worktree's repository, peeling branches to commits.
pub fn read_head(repo: &gix::Repository) -> Result<Head> {
    let head = repo.head()?;
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let name = branch_name(reference.name.as_bstr());
            let mut reference = reference.attach(repo);
            let oid = reference.peel_to_id()?.detach();
            Head::Branch {
                name,
                oid: oid.to_string(),
            }
        }
        gix::head::Kind::Unborn(name) => Head::Unborn {
            name: branch_name(name.as_bstr()),
        },
        gix::head::Kind::Detached { target, peeled } => Head::Detached {
            oid: peeled.unwrap_or(target).to_string(),
        },
    })
}

/// `refs/heads/x` → `x`; other refs keep their full name.
pub fn branch_name(full: &gix::bstr::BStr) -> String {
    let full = String::from_utf8_lossy(full);
    full.strip_prefix("refs/heads/")
        .map_or_else(|| full.clone().into_owned(), str::to_owned)
}

pub fn in_progress(repo: &gix::Repository) -> Option<InProgress> {
    use gix::state::InProgress as Git;
    Some(match repo.state()? {
        Git::ApplyMailbox | Git::ApplyMailboxRebase => InProgress::ApplyMailbox,
        Git::Bisect => InProgress::Bisect,
        Git::CherryPick | Git::CherryPickSequence => InProgress::CherryPick,
        Git::Merge => InProgress::Merge,
        Git::Rebase | Git::RebaseInteractive => InProgress::Rebase,
        Git::Revert | Git::RevertSequence => InProgress::Revert,
    })
}
