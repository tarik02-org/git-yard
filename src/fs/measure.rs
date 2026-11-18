//! Size, modification and storage facts of a worktree directory.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jwalk::{Parallelism, WalkDirGeneric};
use rayon::ThreadPool;

use crate::core::facts::StorageFacts;
use crate::core::ids::Timestamp;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    meta: bool,
    nested_repo: bool,
}

/// Per-entry state from the parallel readers: own metadata (never following
/// symlinks), and whether the directory is a nested repository.
type Walk = WalkDirGeneric<((), (Option<fs::Metadata>, Seen))>;

/// Walks `root` on `pool` without following symlinks or crossing mounts.
/// The top-level `.git` is skipped: a pointer file for linked worktrees,
/// shared storage for main ones. Returns `Ok(None)` when `keep_going`
/// turned false: a cancelled walk is not a measurement. `hold` runs on the
/// reader threads before each directory read and may block to pause.
pub fn measure(
    root: &Path,
    pool: &Arc<ThreadPool>,
    mut progress: impl FnMut(&StorageFacts),
    keep_going: impl Fn() -> bool + Send + Sync + 'static,
    hold: impl Fn() + Send + Sync + 'static,
) -> io::Result<Option<StorageFacts>> {
    let root_meta = fs::symlink_metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", root.display()),
        ));
    }
    let device = root_meta.dev();
    let root_path: Arc<Path> = Arc::from(root);
    let keep_going = Arc::new(keep_going);
    let reader_keep_going = keep_going.clone();

    let walk = Walk::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(Parallelism::RayonExistingPool {
            pool: pool.clone(),
            busy_timeout: None,
        })
        .process_read_dir(move |_, dir, _, children| {
            hold();
            if !reader_keep_going() {
                children.clear();
                return;
            }
            let at_root = dir == &*root_path;
            children.retain(|child| {
                !(at_root && child.as_ref().is_ok_and(|entry| entry.file_name == ".git"))
            });
            for entry in children.iter_mut().flatten() {
                let meta = fs::symlink_metadata(entry.path()).ok();
                let mut seen = Seen {
                    meta: meta.is_some(),
                    nested_repo: false,
                };
                if let Some(meta) = &meta
                    && meta.is_dir()
                {
                    if meta.dev() != device {
                        entry.read_children = None;
                    } else if entry.file_name == ".git" {
                        seen.nested_repo = true;
                    } else {
                        let path = entry.path();
                        seen.nested_repo = path.join("HEAD").is_file()
                            && path.join("objects").is_dir()
                            && path.join("refs").is_dir();
                    }
                } else if entry.file_name == ".git" {
                    seen.nested_repo = true;
                }
                entry.client_state = (meta, seen);
            }
        });

    let mut facts = StorageFacts {
        newest_mtime: Some(root_meta.mtime()),
        ..StorageFacts::default()
    };
    let mut hardlinks = HashSet::new();
    let mut last_report = Instant::now();

    for entry in walk {
        let Ok(entry) = entry else {
            facts.errors += 1;
            continue;
        };
        if entry.depth == 0 {
            continue;
        }
        let (meta, seen) = &entry.client_state;
        let Some(meta) = meta.as_ref().filter(|_| seen.meta) else {
            facts.errors += 1;
            continue;
        };
        if seen.nested_repo {
            facts.nested_repos += 1;
        }
        if meta.is_dir() && meta.dev() != device {
            facts.mounts += 1;
            continue;
        }
        facts.entries += 1;
        bump(&mut facts.newest_mtime, meta.mtime());

        let linked = meta.is_file() && meta.nlink() > 1;
        if linked {
            facts.hardlinked += meta.len();
        }
        if !linked || hardlinks.insert((meta.dev(), meta.ino())) {
            facts.logical += meta.len();
            facts.allocated += meta.blocks() * 512;
        }

        if facts.entries.is_multiple_of(1024) && last_report.elapsed() >= PROGRESS_INTERVAL {
            progress(&facts);
            last_report = Instant::now();
        }
    }
    Ok(keep_going().then_some(facts))
}

fn bump(newest: &mut Option<Timestamp>, value: Timestamp) {
    *newest = Some(newest.map_or(value, |current| current.max(value)));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn pool() -> Arc<ThreadPool> {
        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .unwrap(),
        )
    }

    #[test]
    fn counts_files_once_skips_symlinks_and_finds_nested_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/a"), vec![0u8; 1000]).unwrap();
        fs::hard_link(root.join("sub/a"), root.join("b")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("big"), vec![0u8; 100_000]).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        fs::create_dir_all(root.join("vendor/lib/.git")).unwrap();

        let facts = measure(root, &pool(), |_| {}, || true, || {})
            .unwrap()
            .unwrap();
        assert_eq!(facts.hardlinked, 2000);
        assert_eq!(facts.nested_repos, 1);
        assert!(facts.logical >= 1000 && facts.logical < 100_000);
    }

    #[test]
    fn cancelled_walk_is_not_a_measurement() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let flag = stop.clone();
        let result = measure(
            dir.path(),
            &pool(),
            |_| {},
            move || !flag.load(Ordering::Relaxed),
            || {},
        )
        .unwrap();
        assert_eq!(result, None);
    }
}
