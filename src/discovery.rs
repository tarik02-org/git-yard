//! Finds repositories under the scan roots and enumerates their worktree
//! registrations, including linked worktrees outside the roots.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use git2::{Repository, WorktreeLockStatus};

use crate::model::{
    BaseRef, CandidateId, DirState, Kind, Lock, Registration, Seed, Timestamp, to_timestamp,
};

#[derive(Debug)]
pub struct FoundRepo {
    pub common_dir: PathBuf,
    pub root: PathBuf,
}

/// Canonicalises roots and drops roots nested inside other roots.
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
/// is found. Does not follow symlinks or cross mount points.
pub fn walk(
    roots: &[PathBuf],
    max_depth: usize,
    skip_dirs: &[String],
    mut found: impl FnMut(FoundRepo),
) {
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(root_meta) = fs::metadata(root) else {
            continue;
        };
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
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

/// Returns the canonical common Git directory if `dir` is a worktree or a
/// bare repository.
fn repository_at(dir: &Path) -> Option<PathBuf> {
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
    let looks_bare =
        dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir();
    if looks_bare {
        return fs::canonicalize(dir).ok();
    }
    None
}

/// Reads a `.git` file (`gitdir: <path>`) and resolves the path relative to
/// the file's directory.
pub fn read_gitdir_file(dot_git: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dot_git).ok()?;
    let target = text.strip_prefix("gitdir:")?.trim();
    let base = dot_git.parent()?;
    Some(base.join(target))
}

/// Resolves a linked worktree's administrative directory to the repository's
/// common directory via its `commondir` file.
fn common_dir_of(gitdir: &Path) -> PathBuf {
    match fs::read_to_string(gitdir.join("commondir")) {
        Ok(text) => gitdir.join(text.trim()),
        Err(_) => gitdir.to_path_buf(),
    }
}

pub struct RepoScan {
    pub seeds: Vec<Seed>,
}

/// Opens the repository once and produces a seed for every registration.
pub fn scan_repo(found: &FoundRepo, roots: &[PathBuf]) -> Result<RepoScan, git2::Error> {
    let repo = Repository::open(&found.common_dir)?;
    let common = found.common_dir.clone();
    let base = base_refs(&repo);

    let main_path = main_worktree_path(&repo, &common);
    let repo_label = label(&main_path, &found.root, roots);

    let mut seeds = Vec::new();

    let is_bare = repo.is_bare();
    let main_kind = if is_bare {
        Kind::BareRepository
    } else {
        Kind::MainWorktree
    };
    seeds.push(Seed {
        id: CandidateId {
            repo: common.clone(),
            registration: Registration::Main,
        },
        kind: main_kind,
        dir: dir_state(&main_path),
        path: main_path,
        repo_label: repo_label.clone(),
        gitdir: common.clone(),
        locked: None,
        git_activity: git_activity(&common),
        conflict: None,
        base: base.clone(),
    });

    for name in repo
        .worktrees()?
        .iter()
        .filter_map(|name| name.ok().flatten())
    {
        let gitdir = common.join("worktrees").join(name);
        let (path, locked) = match repo.find_worktree(name) {
            Ok(worktree) => {
                let locked = match worktree.is_locked() {
                    Ok(WorktreeLockStatus::Locked(reason)) => Some(Lock { reason }),
                    _ => None,
                };
                (worktree.path().to_path_buf(), locked)
            }
            Err(_) => (registered_path(&gitdir).unwrap_or_default(), None),
        };
        let dir = dir_state(&path);
        let conflict = match dir {
            DirState::Present { .. } => ownership_conflict(&path, &gitdir),
            DirState::Missing => None,
        };
        seeds.push(Seed {
            id: CandidateId {
                repo: common.clone(),
                registration: Registration::Linked(name.to_owned()),
            },
            kind: Kind::LinkedWorktree,
            path,
            repo_label: repo_label.clone(),
            gitdir: gitdir.clone(),
            locked,
            dir,
            git_activity: git_activity(&gitdir),
            conflict,
            base: base.clone(),
        });
    }

    Ok(RepoScan { seeds })
}

fn main_worktree_path(repo: &Repository, common: &Path) -> PathBuf {
    if let Some(workdir) = repo.workdir() {
        // libgit2 reports the workdir with a trailing separator.
        return workdir.components().collect();
    }
    common.to_path_buf()
}

/// Path recorded in a registration's `gitdir` file (`<worktree>/.git`).
fn registered_path(gitdir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(gitdir.join("gitdir")).ok()?;
    let dot_git = PathBuf::from(text.trim());
    dot_git.parent().map(Path::to_path_buf)
}

pub fn dir_state(path: &Path) -> DirState {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => DirState::Present {
            dev: meta.dev(),
            ino: meta.ino(),
        },
        _ => DirState::Missing,
    }
}

/// A worktree directory whose `.git` file points at a different registration
/// belongs to someone else.
pub fn ownership_conflict(path: &Path, gitdir: &Path) -> Option<String> {
    let dot_git = path.join(".git");
    let Some(target) = read_gitdir_file(&dot_git) else {
        return Some(format!(
            "{} is not a linked worktree checkout",
            dot_git.display()
        ));
    };
    let target = fs::canonicalize(&target).unwrap_or(target);
    let expected = fs::canonicalize(gitdir).unwrap_or_else(|_| gitdir.to_path_buf());
    if target == expected {
        None
    } else {
        Some(format!("directory belongs to {}", target.display()))
    }
}

fn git_activity(gitdir: &Path) -> Option<Timestamp> {
    ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|name| fs::metadata(gitdir.join(name)).ok()?.modified().ok())
        .map(to_timestamp)
        .max()
}

/// The branches integration is measured against: the remote default branch
/// and its local counterpart, or conventional names when no remote HEAD exists.
pub fn base_refs(repo: &Repository) -> Vec<BaseRef> {
    let remote_default = repo
        .find_reference("refs/remotes/origin/HEAD")
        .ok()
        .and_then(|reference| {
            reference
                .symbolic_target()
                .ok()
                .flatten()
                .map(str::to_owned)
        })
        .and_then(|target| {
            target
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_owned)
        });

    let names: Vec<String> = match remote_default {
        Some(name) => vec![name],
        None => ["main", "master", "trunk", "develop"]
            .into_iter()
            .filter(|name| {
                repo.refname_to_id(&format!("refs/heads/{name}")).is_ok()
                    || repo
                        .refname_to_id(&format!("refs/remotes/origin/{name}"))
                        .is_ok()
            })
            .take(1)
            .map(String::from)
            .collect(),
    };

    let mut result = Vec::new();
    for branch in names {
        for (refname, short) in [
            (format!("refs/heads/{branch}"), branch.clone()),
            (
                format!("refs/remotes/origin/{branch}"),
                format!("origin/{branch}"),
            ),
        ] {
            let commit = repo
                .find_reference(&refname)
                .and_then(|reference| reference.peel_to_commit());
            if let Ok(commit) = commit {
                result.push(BaseRef {
                    name: short,
                    oid: commit.id().to_string(),
                    branch: branch.clone(),
                });
            }
        }
    }
    result
}

/// `namespace/project` relative to the root the repository was found in, or
/// a home-relative path when the main worktree lies outside every root.
fn label(main_path: &Path, found_root: &Path, roots: &[PathBuf]) -> String {
    let relative = std::iter::once(found_root)
        .chain(roots.iter().map(PathBuf::as_path))
        .find_map(|root| main_path.strip_prefix(root).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    let text = match relative {
        Some(relative) => relative.display().to_string(),
        None => display_path(main_path),
    };
    let text = text
        .strip_suffix(".git")
        .map(|text| text.trim_end_matches('/').to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or(text);
    collapse_repeated_tail(&text)
}

/// `bb/charts/charts` → `bb/charts`: the `<namespace>/<project>/<project>`
/// layout repeats the project name for the main worktree directory.
fn collapse_repeated_tail(label: &str) -> String {
    match label.rsplit_once('/') {
        Some((head, last)) if head.rsplit('/').next() == Some(last) => head.to_owned(),
        _ => label.to_owned(),
    }
}

pub fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Detects registrations that refer to the same physical directory.
#[derive(Default)]
pub struct ConflictTracker {
    by_inode: HashMap<(u64, u64), CandidateId>,
}

impl ConflictTracker {
    /// Records the seed and returns the earlier registration it collides with.
    pub fn check(&mut self, seed: &Seed) -> Option<CandidateId> {
        if seed.kind == Kind::BareRepository {
            return None;
        }
        let DirState::Present { dev, ino } = seed.dir else {
            return None;
        };
        match self.by_inode.get(&(dev, ino)) {
            Some(other) if other != &seed.id => Some(other.clone()),
            Some(_) => None,
            None => {
                self.by_inode.insert((dev, ino), seed.id.clone());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_repeated_project_directory() {
        assert_eq!(collapse_repeated_tail("bb/charts/charts"), "bb/charts");
        assert_eq!(collapse_repeated_tail("bb/charts/web"), "bb/charts/web");
        assert_eq!(collapse_repeated_tail("charts"), "charts");
    }
}
