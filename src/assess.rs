//! Git assessment of a single registration: HEAD, integration with the base
//! branch, working-tree state, and unreachable commits.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::model::{BaseRef, DirState, Dirty, GitFacts, Head, Integration, Kind, Seed};

/// Reuses opened repositories within one worker thread.
#[derive(Default)]
pub struct RepoCache {
    repos: HashMap<PathBuf, Repository>,
}

const REPO_CACHE_LIMIT: usize = 32;

impl RepoCache {
    pub fn get(&mut self, common_dir: &Path) -> Result<&Repository> {
        if !self.repos.contains_key(common_dir) {
            if self.repos.len() >= REPO_CACHE_LIMIT {
                self.repos.clear();
            }
            let repo = Repository::open(common_dir)
                .with_context(|| format!("opening {}", common_dir.display()))?;
            self.repos.insert(common_dir.to_path_buf(), repo);
        }
        Ok(&self.repos[common_dir])
    }
}

pub fn assess(seed: &Seed, cache: &mut RepoCache) -> Result<GitFacts> {
    // Main worktrees are never removed in v1, so their status cannot affect
    // a decision; it is the most expensive check on large repositories.
    let inspect_status =
        seed.kind == Kind::LinkedWorktree && matches!(seed.dir, DirState::Present { .. });

    std::thread::scope(|scope| {
        // Status walks the working tree while the graph checks run here; on
        // large repositories they take comparable time.
        let status = inspect_status.then(|| scope.spawn(|| dirty(&seed.path)));

        let repo = cache.get(&seed.id.repo)?;
        if seed.kind == Kind::LinkedWorktree {
            ensure_removal_storage(&seed.gitdir, &seed.path)?;
        }
        let head = read_head(repo, &seed.gitdir, seed.kind)?;
        let tip = head.oid().map(Oid::from_str).transpose()?;
        let commit = tip.map(|oid| repo.find_commit(oid)).transpose()?;
        let last_commit = commit.as_ref().map(|commit| commit.time().seconds());
        let subject = commit
            .as_ref()
            .and_then(|commit| commit.summary().ok().flatten().map(str::to_owned));
        let integration = match (&head, tip) {
            (_, None) => Integration::Unborn,
            (head, Some(tip)) => integration(repo, head.branch(), tip, &seed.base)?,
        };
        let unreachable_commits = match (&head, tip) {
            (Head::Detached { .. }, Some(tip)) => has_unreachable_commits(repo, tip)?,
            _ => false,
        };
        let dirty = match status {
            Some(handle) => Some(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("status check panicked"))??,
            ),
            None => None,
        };

        Ok(GitFacts {
            head,
            last_commit,
            subject,
            integration,
            dirty,
            unreachable_commits,
        })
    })
}

/// Reads HEAD from the registration's administrative directory so missing
/// worktree directories can still be assessed.
pub fn read_head(repo: &Repository, gitdir: &Path, kind: Kind) -> Result<Head> {
    let text = fs::read_to_string(gitdir.join("HEAD"))
        .with_context(|| format!("reading {}/HEAD", gitdir.display()))?;
    let text = text.trim();
    if let Some(target) = text.strip_prefix("ref:") {
        let target = target.trim();
        let name = target
            .strip_prefix("refs/heads/")
            .unwrap_or(target)
            .to_owned();
        return Ok(match repo.refname_to_id(target) {
            Ok(oid) => Head::Branch {
                name,
                oid: oid.to_string(),
            },
            Err(error) if error.code() == git2::ErrorCode::NotFound => Head::Unborn { name },
            Err(error) => return Err(error.into()),
        });
    }
    let oid = Oid::from_str(text).with_context(|| {
        format!(
            "{kind:?} HEAD in {} is neither a ref nor an oid",
            gitdir.display()
        )
    })?;
    Ok(Head::Detached {
        oid: oid.to_string(),
    })
}

fn integration(
    repo: &Repository,
    branch: Option<&str>,
    tip: Oid,
    base: &[BaseRef],
) -> Result<Integration> {
    if base.is_empty() {
        return Ok(Integration::NoBase);
    }
    if let Some(branch) = branch
        && base.iter().any(|base| base.branch == branch)
    {
        return Ok(Integration::IsBase);
    }

    for base in base {
        let base_oid = Oid::from_str(&base.oid)?;
        if base_oid == tip || repo.graph_descendant_of(base_oid, tip)? {
            return Ok(Integration::Merged {
                into: base.name.clone(),
            });
        }
    }
    for base in base {
        let base_oid = Oid::from_str(&base.oid)?;
        if changes_present(repo, tip, base_oid)? {
            return Ok(Integration::ChangesPresent {
                into: base.name.clone(),
            });
        }
    }

    let first = &base[0];
    let (ahead, _) = repo.graph_ahead_behind(tip, Oid::from_str(&first.oid)?)?;
    Ok(Integration::NotIntegrated {
        base: first.name.clone(),
        ahead,
    })
}

/// True when every path the branch changed since its merge base has identical
/// content and mode in the base. This recognises squash and rebase merges
/// without writing merge results into the object database. It is stricter
/// than a merge simulation: later edits to the same paths on the base make it
/// report "not integrated", which errs on the side of keeping work.
fn changes_present(repo: &Repository, tip: Oid, base: Oid) -> Result<bool> {
    let merge_base = match repo.merge_base(tip, base) {
        Ok(oid) => oid,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let tip_tree = repo.find_commit(tip)?.tree()?;
    let base_tree = repo.find_commit(base)?.tree()?;
    let merge_base_tree = repo.find_commit(merge_base)?.tree()?;

    let diff = repo.diff_tree_to_tree(Some(&merge_base_tree), Some(&tip_tree), None)?;
    for delta in diff.deltas() {
        for path in [delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
        {
            let in_tip = tip_tree.get_path(path).ok();
            let in_base = base_tree.get_path(path).ok();
            let same = match (in_tip, in_base) {
                (None, None) => true,
                (Some(a), Some(b)) => a.id() == b.id() && a.filemode() == b.filemode(),
                _ => false,
            };
            if !same {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Whether `tip` has commits no reference reaches. Removing a detached
/// worktree would leave those commits only in the reflog of a deleted
/// registration.
pub fn has_unreachable_commits(repo: &Repository, tip: Oid) -> Result<bool> {
    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    for reference in repo.references()?.flatten() {
        if let Ok(commit) = reference.peel_to_commit() {
            walk.hide(commit.id())?;
        }
    }
    Ok(walk.next().transpose()?.is_some())
}

/// Working-tree status via gitoxide, which checks tracked files in parallel;
/// libgit2 was 2–3× slower on large worktrees. Read-only: refreshed stat
/// data is not written back to the index.
pub fn dirty(worktree: &Path) -> Result<Dirty> {
    use gix::status::plumbing::index_as_worktree::EntryStatus;
    use gix::status::{Item, Submodule, UntrackedFiles, index_worktree};

    let repo =
        gix::open(worktree).with_context(|| format!("opening worktree {}", worktree.display()))?;
    let items = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Collapsed)
        .index_worktree_rewrites(None)
        .index_worktree_submodules(Submodule::Given {
            ignore: gix::submodule::config::Ignore::None,
            check_dirty: false,
        })
        .into_iter(Vec::new())?;

    let mut result = Dirty::default();
    let mut evidence = Vec::new();
    for item in items {
        let item = item?;
        let detail = match &item {
            Item::TreeIndex(change) => {
                result.changed += 1;
                Some(format!("{change:?}"))
            }
            Item::IndexWorktree(index_worktree::Item::Modification { status, .. }) => {
                match status {
                    EntryStatus::Conflict { .. } => {
                        result.conflicted += 1;
                        Some(format!("{status:?}"))
                    }
                    EntryStatus::Change(_) | EntryStatus::IntentToAdd => {
                        result.changed += 1;
                        Some("changed".to_owned())
                    }
                    // Only stat data differs; the content is unchanged.
                    EntryStatus::NeedsUpdate(_) => None,
                }
            }
            Item::IndexWorktree(index_worktree::Item::DirectoryContents { entry, .. }) => {
                if entry.status == gix::dir::entry::Status::Untracked {
                    result.untracked += 1;
                    Some("untracked".to_owned())
                } else {
                    None
                }
            }
            Item::IndexWorktree(index_worktree::Item::Rewrite { .. }) => {
                unreachable!("rewrite tracking is disabled")
            }
        };
        if let Some(detail) = detail {
            evidence.push((gix::path::from_bstr(item.location())?.into_owned(), detail));
        }
    }
    if !evidence.is_empty() {
        evidence.sort();
        let ignore = Repository::open(worktree)?;
        let mut hash = gix::hash::hasher(gix::hash::Kind::Sha1);
        for (path, detail) in evidence {
            hash_field(&mut hash, path.as_os_str().as_bytes());
            hash_field(&mut hash, detail.as_bytes());
            fingerprint_path(worktree, &ignore, &worktree.join(path), &mut hash)?;
        }
        result
            .fingerprint
            .copy_from_slice(hash.try_finalize()?.as_bytes());
    }
    Ok(result)
}

fn hash_field(hash: &mut gix::hash::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn fingerprint_path(
    worktree: &Path,
    ignore: &Repository,
    path: &Path,
    hash: &mut gix::hash::Hasher,
) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash_field(hash, b"missing");
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
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
    } else if meta.is_dir() {
        if path.join(".git").exists() {
            let repo = Repository::open(path)?;
            hash_field(hash, repo.head()?.peel_to_commit()?.id().as_bytes());
            hash_field(hash, &dirty(path)?.fingerprint);
        } else {
            let mut children = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                if ignore.status_should_ignore(child.path().strip_prefix(worktree)?)? {
                    continue;
                }
                hash_field(hash, child.file_name().as_bytes());
                fingerprint_path(worktree, ignore, &child.path(), hash)?;
            }
        }
    } else {
        anyhow::bail!("cannot safely fingerprint special file {}", path.display());
    }
    Ok(())
}

/// Pruning a linked registration also destroys its private submodule
/// repositories, including commits that may exist nowhere else.
pub fn ensure_removal_storage(gitdir: &Path, path: &Path) -> Result<()> {
    anyhow::ensure!(
        !gitdir.join("modules").try_exists()?,
        "worktree registration contains private submodule storage; left untouched"
    );
    let root = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => meta,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut directories = vec![path.to_path_buf()];
    while let Some(directory) = directories.pop() {
        if directory != path {
            let bare = directory.join("HEAD").is_file()
                && directory.join("objects").is_dir()
                && directory.join("refs").is_dir();
            anyhow::ensure!(
                !directory.join(".git").try_exists()? && !bare,
                "directory contains a nested repository or worktree: {}; left untouched",
                directory.display()
            );
        }
        for child in fs::read_dir(&directory)? {
            let child = child?;
            if child.file_name() == ".git" {
                continue;
            }
            let meta = child.metadata()?;
            if meta.is_dir() {
                use std::os::unix::fs::MetadataExt;
                anyhow::ensure!(
                    meta.dev() == root.dev(),
                    "directory contains a mounted filesystem: {}; left untouched",
                    child.path().display()
                );
                directories.push(child.path());
            }
        }
    }
    Ok(())
}
