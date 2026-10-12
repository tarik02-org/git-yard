//! Working-tree status as evidence: counts and a hash of every change, so
//! removal can tell whether anything moved since the user decided.

use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use gix::status::plumbing::index_as_worktree::EntryStatus;
use gix::status::{Item, Submodule, UntrackedFiles, index_worktree};

use crate::core::facts::StatusEvidence;

enum Detail {
    Staged(String),
    Changed,
    Conflicted(String),
    Untracked,
}

/// Full status of a present worktree. Ignored files are counted, not
/// hashed. `Ok(None)` when interrupted.
pub fn worktree_status(
    repo: &gix::Repository,
    interrupt: Arc<AtomicBool>,
) -> Result<Option<StatusEvidence>> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("repository has no working directory"))?
        .to_path_buf();
    let items = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_submodules(Submodule::Given {
            ignore: gix::submodule::config::Ignore::None,
            check_dirty: false,
        })
        .dirwalk_options(|options| {
            options.emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
        })
        .should_interrupt_owned(interrupt.clone())
        .into_iter(Vec::new())?;

    let mut evidence = StatusEvidence::default();
    let mut details = Vec::new();
    for item in items {
        let item = item?;
        let detail = match &item {
            Item::TreeIndex(change) => Some(Detail::Staged(format!("{change:?}"))),
            Item::IndexWorktree(index_worktree::Item::Modification { status, .. }) => {
                match status {
                    EntryStatus::Conflict { .. } => Some(Detail::Conflicted(format!("{status:?}"))),
                    EntryStatus::Change(_) | EntryStatus::IntentToAdd => Some(Detail::Changed),
                    // Only stat data differs; the content is unchanged.
                    EntryStatus::NeedsUpdate(_) => None,
                }
            }
            Item::IndexWorktree(index_worktree::Item::DirectoryContents { entry, .. }) => {
                match entry.status {
                    gix::dir::entry::Status::Untracked => Some(Detail::Untracked),
                    gix::dir::entry::Status::Ignored(_) => {
                        evidence.ignored += 1;
                        None
                    }
                    _ => None,
                }
            }
            Item::IndexWorktree(index_worktree::Item::Rewrite { .. }) => {
                unreachable!("rewrite tracking is disabled")
            }
        };
        if let Some(detail) = detail {
            details.push((item.location().to_owned(), detail));
        }
    }
    if interrupt.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // Producers run in parallel, so order is restored before hashing.
    details.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = gix::hash::hasher(gix::hash::Kind::Sha1);
    for (location, detail) in details {
        hash_field(&mut hash, &location);
        let path = workdir.join(gix::path::from_bstr(AsRef::<gix::bstr::BStr>::as_ref(
            &location,
        ))?);
        match detail {
            Detail::Staged(change) => {
                evidence.staged += 1;
                hash_field(&mut hash, b"staged");
                hash_field(&mut hash, change.as_bytes());
            }
            Detail::Conflicted(status) => {
                evidence.conflicted += 1;
                hash_field(&mut hash, b"conflicted");
                hash_field(&mut hash, status.as_bytes());
                hash_content(&mut hash, &path)?;
            }
            Detail::Changed => {
                evidence.changed += 1;
                hash_field(&mut hash, b"changed");
                hash_content(&mut hash, &path)?;
            }
            Detail::Untracked => {
                evidence.untracked += 1;
                hash_field(&mut hash, b"untracked");
                hash_content(&mut hash, &path)?;
            }
        }
    }
    evidence
        .hash
        .copy_from_slice(hash.try_finalize()?.as_bytes());
    Ok(Some(evidence))
}

/// Staged changes of a worktree whose directory is gone: its index against
/// its HEAD tree. Nothing else about it can be known.
pub fn staged_status(repo: &gix::Repository) -> Result<StatusEvidence> {
    let index = repo.index_or_empty()?;
    let tree = repo.head_tree_id_or_empty()?;
    let mut changes = Vec::new();
    repo.tree_index_status(
        &tree,
        &index,
        None,
        gix::status::tree_index::TrackRenames::Disabled,
        |change, _, _| {
            changes.push(format!("{change:?}"));
            Ok(gix::diff::index::Action::Continue(()))
        },
    )?;
    changes.sort();
    let mut hash = gix::hash::hasher(gix::hash::Kind::Sha1);
    for change in &changes {
        hash_field(&mut hash, change.as_bytes());
    }
    let mut evidence = StatusEvidence {
        staged: changes.len(),
        ..StatusEvidence::default()
    };
    evidence
        .hash
        .copy_from_slice(hash.try_finalize()?.as_bytes());
    Ok(evidence)
}

fn hash_field(hash: &mut gix::hash::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Mode and content of a file or symlink; directories contribute their
/// type only (their files are reported individually).
fn hash_content(hash: &mut gix::hash::Hasher, path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash_field(hash, b"missing");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    hash_field(hash, &meta.permissions().mode().to_le_bytes());
    if meta.is_symlink() {
        hash_field(hash, fs::read_link(path)?.as_os_str().as_bytes());
    } else if meta.is_file() {
        hash_field(hash, &meta.len().to_le_bytes());
        let mut file = fs::File::open(path)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
    } else {
        hash_field(hash, b"not a file");
    }
    Ok(())
}
