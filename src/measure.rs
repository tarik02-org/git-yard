//! Size and modification measurement of a worktree directory.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jwalk::{Parallelism, WalkDirGeneric};
use rayon::ThreadPool;

use crate::model::{Timestamp, Usage};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Per-entry state filled in by the parallel directory readers: the entry's
/// own metadata (never following symlinks), or `None` if it could not be read.
type Walk = WalkDirGeneric<((), Option<fs::Metadata>)>;

/// Walks `root` in parallel on `pool`, without following symlinks or
/// crossing mount points. The top-level `.git` entry is skipped: for linked
/// worktrees it is a pointer file, for main worktrees it is shared repository
/// storage that is not charged to the worktree. `progress` receives partial
/// totals periodically; returning `false` from `keep_going` stops the walk.
/// `hold` runs on the reader threads before each directory read and may
/// block to pause the walk without losing its progress.
pub fn measure(
    root: &Path,
    pool: &Arc<ThreadPool>,
    mut progress: impl FnMut(&Usage),
    keep_going: impl Fn() -> bool,
    hold: impl Fn() + Send + Sync + 'static,
) -> io::Result<Usage> {
    let root_meta = fs::symlink_metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", root.display()),
        ));
    }
    let device = root_meta.dev();
    let root_path: Arc<Path> = Arc::from(root);

    let walk = Walk::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(Parallelism::RayonExistingPool {
            pool: pool.clone(),
            // Callers never run on the pool, so a free thread always exists.
            busy_timeout: None,
        })
        .process_read_dir(move |_, dir, _, children| {
            hold();
            let at_root = dir == &*root_path;
            children.retain(|child| {
                !(at_root && child.as_ref().is_ok_and(|entry| entry.file_name == ".git"))
            });
            for entry in children.iter_mut().flatten() {
                let meta = fs::symlink_metadata(entry.path()).ok();
                let other_device = meta
                    .as_ref()
                    .is_some_and(|meta| meta.is_dir() && meta.dev() != device);
                if other_device {
                    entry.read_children = None;
                }
                entry.client_state = meta;
            }
        });

    let mut usage = Usage {
        newest_mtime: Some(root_meta.mtime()),
        ..Usage::default()
    };
    let mut hardlinks = HashSet::new();
    let mut last_report = Instant::now();

    for entry in walk {
        let Ok(entry) = entry else {
            usage.errors += 1;
            continue;
        };
        if entry.depth == 0 {
            continue;
        }
        let Some(meta) = &entry.client_state else {
            usage.errors += 1;
            continue;
        };
        if meta.is_dir() && meta.dev() != device {
            usage.skipped_mounts += 1;
            continue;
        }
        usage.entries += 1;
        bump(&mut usage.newest_mtime, meta.mtime());

        let counted =
            !(meta.is_file() && meta.nlink() > 1) || hardlinks.insert((meta.dev(), meta.ino()));
        if meta.is_file() && meta.nlink() > 1 {
            usage.hardlinked += meta.len();
        }
        if counted {
            usage.logical += meta.len();
            usage.allocated += meta.blocks() * 512;
        }

        if usage.entries.is_multiple_of(1024) && last_report.elapsed() >= PROGRESS_INTERVAL {
            if !keep_going() {
                return Ok(usage);
            }
            progress(&usage);
            last_report = Instant::now();
        }
    }
    Ok(usage)
}

fn bump(newest: &mut Option<Timestamp>, value: Timestamp) {
    *newest = Some(newest.map_or(value, |current| current.max(value)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_files_once_and_skips_git_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/a"), vec![0u8; 1000]).unwrap();
        fs::hard_link(root.join("sub/a"), root.join("b")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("big"), vec![0u8; 100_000]).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .unwrap(),
        );
        let usage = measure(root, &pool, |_| {}, || true, || {}).unwrap();
        // sub/a and b are one inode; the symlink counts as itself.
        let link_len = fs::symlink_metadata(root.join("link")).unwrap().len();
        let dir_len = fs::symlink_metadata(root.join("sub")).unwrap().len();
        assert_eq!(usage.logical, 1000 + link_len + dir_len);
        assert_eq!(usage.hardlinked, 2000);
        assert_eq!(usage.entries, 4);
        assert!(usage.newest_mtime.is_some());
    }
}
