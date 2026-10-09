//! Git assessment of a single registration: HEAD, integration with the base
//! branch, working-tree state, and unreachable commits.

use std::collections::HashMap;
use std::fs;
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
        let head = read_head(repo, &seed.gitdir, seed.kind)?;
        let tip = head.oid().map(Oid::from_str).transpose()?;
        let commit = tip.map(|oid| repo.find_commit(oid)).transpose()?;
        let last_commit = commit.as_ref().map(|commit| commit.time().seconds());
        let subject = commit
            .as_ref()
            .and_then(|commit| commit.summary().map(str::to_owned));
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
            ignore: gix::submodule::config::Ignore::All,
            check_dirty: false,
        })
        .into_iter(Vec::new())?;

    let mut result = Dirty::default();
    for item in items {
        match item? {
            Item::TreeIndex(_) => result.changed += 1,
            Item::IndexWorktree(index_worktree::Item::Modification { status, .. }) => {
                match status {
                    EntryStatus::Conflict { .. } => result.conflicted += 1,
                    EntryStatus::Change(_) | EntryStatus::IntentToAdd => result.changed += 1,
                    // Only stat data differs; the content is unchanged.
                    EntryStatus::NeedsUpdate(_) => {}
                }
            }
            Item::IndexWorktree(index_worktree::Item::DirectoryContents { entry, .. }) => {
                if entry.status == gix::dir::entry::Status::Untracked {
                    result.untracked += 1;
                }
            }
            Item::IndexWorktree(index_worktree::Item::Rewrite { .. }) => result.changed += 1,
        }
    }
    Ok(result)
}
