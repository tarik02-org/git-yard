//! A repository's worktree registrations, shared by cleanup and the picker.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::core::candidate::{Kind, Seed};
use crate::core::facts::{DirState, Lock};
use crate::core::format;
use crate::core::ids::{RepoKey, Timestamp, WorktreeKey, YardPath, to_timestamp};
use crate::fs::dirstate::dir_state;
use crate::fs::paths::normalize;
use crate::fs::walk::{FoundRepo, read_gitdir_file};

#[derive(Clone, Debug)]
pub struct Inventory {
    pub repo: RepoKey,
    pub label: String,
    /// Main worktree, or the repository itself when bare.
    pub main_path: PathBuf,
    /// The main registration first, then linked ones by admin id.
    pub seeds: Vec<Seed>,
}

/// Reads every registration. A registration that cannot be read becomes a
/// row with `registration_error`, never a missing one.
pub fn inventory(
    repo: &gix::Repository,
    found: &FoundRepo,
    roots: &[PathBuf],
) -> Result<Inventory> {
    let common = found.common_dir.clone();
    let key = RepoKey::new(&common);
    let bare = repo.is_bare();
    let main_path = match repo.workdir() {
        Some(workdir) if !bare => normalize(workdir),
        _ => common.clone(),
    };
    let label = label(&main_path, &found.root, roots);
    let fetched_at = fs::metadata(common.join("FETCH_HEAD"))
        .and_then(|meta| meta.modified())
        .ok()
        .map(to_timestamp);

    let mut seeds = vec![Seed {
        key: WorktreeKey::main(&key),
        kind: if bare {
            Kind::BareRepository
        } else {
            Kind::MainWorktree
        },
        path: YardPath::new(&main_path),
        repo_label: label.clone(),
        admin_dir: YardPath::new(&common),
        lock: None,
        dir: dir_state(&main_path),
        git_activity: git_activity(&common),
        registration_error: None,
        conflict: None,
        fetched_at,
    }];

    let mut ids: Vec<_> = match fs::read_dir(common.join("worktrees")) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    ids.sort();
    for id in ids {
        let admin_dir = common.join("worktrees").join(&id);
        let (path, registration_error) = match registered_path(repo, &id) {
            Ok(path) => (path, None),
            Err(error) => (admin_dir.clone(), Some(error)),
        };
        let lock = fs::read(admin_dir.join("locked")).ok().map(|reason| Lock {
            reason: Some(String::from_utf8_lossy(reason.trim_ascii()).into_owned())
                .filter(|reason| !reason.is_empty()),
        });
        let dir = if registration_error.is_some() {
            DirState::Unreadable
        } else {
            dir_state(&path)
        };
        let conflict = match dir {
            DirState::Present { .. } => ownership_conflict(&path, &admin_dir),
            _ => None,
        };
        seeds.push(Seed {
            key: WorktreeKey::linked(&key, &id),
            kind: Kind::LinkedWorktree,
            path: YardPath::new(path),
            repo_label: label.clone(),
            git_activity: git_activity(&admin_dir),
            admin_dir: YardPath::new(admin_dir),
            lock,
            dir,
            registration_error,
            conflict,
            fetched_at,
        });
    }
    mark_shared_directories(&mut seeds);
    Ok(Inventory {
        repo: key,
        label,
        main_path,
        seeds,
    })
}

fn registered_path(repo: &gix::Repository, id: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let id = gix::path::os_str_into_bstr(id).map_err(|error| error.to_string())?;
    let proxy = repo
        .worktree_proxy_by_id(id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "missing gitdir file".to_owned())?;
    let base = proxy.base().map_err(|error| error.to_string())?;
    Ok(normalize(&base))
}

/// Opens the repository of one registration; its directory may be missing.
pub fn worktree_repo(repo: &gix::Repository, seed: &Seed) -> Result<gix::Repository> {
    let Some(id) = &seed.key.admin_id else {
        return Ok(repo.clone());
    };
    let id = gix::path::os_str_into_bstr(id.as_os_str())?;
    let proxy = repo
        .worktree_proxy_by_id(id)?
        .ok_or_else(|| anyhow::anyhow!("registration {} disappeared", seed.key))?;
    Ok(proxy.into_repo_with_possibly_inaccessible_worktree()?)
}

/// Registrations that resolve to one directory, by path or by inode, are
/// all conflicts: removing one would destroy the other's checkout.
fn mark_shared_directories(seeds: &mut [Seed]) {
    let mut by_path: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut by_inode: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (index, seed) in seeds.iter().enumerate() {
        if seed.kind == Kind::BareRepository || seed.registration_error.is_some() {
            continue;
        }
        by_path
            .entry(seed.path.to_path_buf())
            .or_default()
            .push(index);
        if let DirState::Present { dev, ino } = seed.dir {
            by_inode.entry((dev, ino)).or_default().push(index);
        }
    }
    let groups: Vec<Vec<usize>> = by_path
        .into_values()
        .chain(by_inode.into_values())
        .filter(|group| group.len() > 1)
        .collect();
    for group in groups {
        for &index in &group {
            let others: Vec<String> = group
                .iter()
                .filter(|&&other| other != index)
                .map(|&other| seeds[other].key.to_string())
                .collect();
            seeds[index].conflict = Some(format!("same directory as {}", others.join(", ")));
        }
    }
}

/// A checkout whose `.git` file points at another registration belongs to
/// someone else.
pub fn ownership_conflict(path: &Path, admin_dir: &Path) -> Option<String> {
    let dot_git = path.join(".git");
    let Some(target) = read_gitdir_file(&dot_git) else {
        return Some(format!(
            "{} is not a linked worktree checkout",
            format::path(&dot_git)
        ));
    };
    let target = fs::canonicalize(&target).unwrap_or(target);
    let expected = fs::canonicalize(admin_dir).unwrap_or_else(|_| admin_dir.to_path_buf());
    if target == expected {
        None
    } else {
        Some(format!("directory belongs to {}", target.display()))
    }
}

fn git_activity(admin_dir: &Path) -> Option<Timestamp> {
    ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|name| fs::metadata(admin_dir.join(name)).ok()?.modified().ok())
        .map(to_timestamp)
        .max()
}

/// `namespace/project` relative to the root the repository was found in,
/// or a home-relative path when the main worktree lies outside every root.
fn label(main_path: &Path, found_root: &Path, roots: &[PathBuf]) -> String {
    let relative = std::iter::once(found_root)
        .chain(roots.iter().map(PathBuf::as_path))
        .find_map(|root| main_path.strip_prefix(root).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    let text = match relative {
        Some(relative) => relative.display().to_string(),
        None => format::path(main_path),
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
