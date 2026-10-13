//! Finds repositories under the scan roots without following symlinks or
//! crossing mount points, and without descending into repositories found.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::fs::paths::normalize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundRepo {
    /// Canonical common Git directory.
    pub common_dir: PathBuf,
    /// The scan root it was found under; labels are relative to it.
    pub root: PathBuf,
}

/// Canonicalizes roots and drops roots nested inside other roots.
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect();
    canonical.sort();
    canonical.dedup();
    let mut result: Vec<PathBuf> = Vec::new();
    for root in canonical {
        if !result.iter().any(|kept| root.starts_with(kept)) {
            result.push(root);
        }
    }
    result
}

/// Walks the roots and reports each distinct repository once, as soon as it
/// is found. `keep_going` is checked between directories.
pub fn walk(
    roots: &[PathBuf],
    max_depth: usize,
    skip_dirs: &[String],
    keep_going: impl Fn() -> bool,
    mut found: impl FnMut(FoundRepo),
) {
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(root_meta) = fs::metadata(root) else {
            continue;
        };
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if !keep_going() {
                return;
            }
            if let Some(common_dir) = repository_at(&dir) {
                if seen.insert(common_dir.clone()) {
                    found(FoundRepo {
                        common_dir,
                        root: root.clone(),
                    });
                }
                continue;
            }
            if depth >= max_depth {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut children = Vec::new();
            for entry in entries.flatten() {
                // DirEntry metadata does not follow symlinks.
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if !meta.is_dir() || meta.dev() != root_meta.dev() {
                    continue;
                }
                let name = entry.file_name();
                if skip_dirs.iter().any(|skipped| name == skipped.as_str()) {
                    continue;
                }
                children.push(entry.path());
            }
            // Reverse-sorted so the stack visits children alphabetically.
            children.sort_by(|a, b| b.cmp(a));
            stack.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }
}

/// The canonical common Git directory if `dir` is a worktree or a bare
/// repository.
pub fn repository_at(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if let Ok(meta) = fs::symlink_metadata(&dot_git) {
        if meta.is_dir() {
            return fs::canonicalize(&dot_git).ok();
        }
        if meta.is_file() {
            let gitdir = read_gitdir_file(&dot_git)?;
            return fs::canonicalize(common_dir_of(&gitdir)).ok();
        }
        return None;
    }
    let looks_bare = dir.join("HEAD").is_file() && dir.join("objects").is_dir();
    if looks_bare && gix::discover::is_git(dir).is_ok() {
        return fs::canonicalize(dir).ok();
    }
    None
}

/// Reads a `.git` file (`gitdir: <path>`), resolving relative paths against
/// the file's directory.
pub fn read_gitdir_file(dot_git: &Path) -> Option<PathBuf> {
    let text = fs::read(dot_git).ok()?;
    let text = text.strip_prefix(b"gitdir:")?.trim_ascii();
    let target = gix::path::from_byte_slice(text).ok()?;
    Some(normalize(&dot_git.parent()?.join(target)))
}

/// A linked worktree's administrative directory resolved to the
/// repository's common directory via its `commondir` file.
fn common_dir_of(gitdir: &Path) -> PathBuf {
    match fs::read(gitdir.join("commondir")) {
        Ok(text) => match gix::path::from_byte_slice(text.trim_ascii()) {
            Ok(relative) => gitdir.join(relative),
            Err(_) => gitdir.to_path_buf(),
        },
        Err(_) => gitdir.to_path_buf(),
    }
}
