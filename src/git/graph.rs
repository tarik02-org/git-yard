//! Commit graph questions: merged, ahead, squash-merged, unreachable.

use std::collections::BTreeMap;

use anyhow::Result;
use gix::ObjectId;
use gix::bstr::BString;
use gix::objs::tree::EntryMode;

use crate::core::facts::{BaseRef, Integration};

pub fn integration(
    repo: &gix::Repository,
    branch: Option<&str>,
    tip: ObjectId,
    bases: &[BaseRef],
) -> Result<Integration> {
    if bases.is_empty() {
        return Ok(Integration::NoBase);
    }
    if let Some(branch) = branch
        && bases.iter().any(|base| base.branch == branch)
    {
        return Ok(Integration::IsBase);
    }
    let parsed: Vec<(&BaseRef, ObjectId)> = bases
        .iter()
        .map(|base| Ok((base, ObjectId::from_hex(base.oid.as_bytes())?)))
        .collect::<Result<_>>()?;
    for (base, oid) in &parsed {
        if is_merged(repo, tip, *oid)? {
            return Ok(Integration::Merged {
                into: base.name.clone(),
            });
        }
    }
    for (base, oid) in &parsed {
        if changes_present(repo, tip, *oid)? {
            return Ok(Integration::ChangesPresent {
                into: base.name.clone(),
            });
        }
    }
    let (base, oid) = parsed[0];
    Ok(Integration::NotIntegrated {
        base: base.name.clone(),
        ahead: ahead(repo, tip, oid)?,
    })
}

/// The tip is the base or one of its ancestors.
pub fn is_merged(repo: &gix::Repository, tip: ObjectId, base: ObjectId) -> Result<bool> {
    if tip == base {
        return Ok(true);
    }
    Ok(repo
        .merge_base(tip, base)?
        .is_some_and(|merge_base| merge_base.detach() == tip))
}

/// Commits reachable from `tip` but not from `base`.
pub fn ahead(repo: &gix::Repository, tip: ObjectId, base: ObjectId) -> Result<usize> {
    let mut count = 0;
    for commit in repo.rev_walk([tip]).with_hidden([base]).all()? {
        commit?;
        count += 1;
    }
    Ok(count)
}

/// True when every path the branch changed since its merge base has the
/// same content and mode in the base: squash and rebase merges. Stricter
/// than a merge simulation; later edits to the same paths on the base make
/// it report "not integrated", which errs on the side of keeping work.
pub fn changes_present(repo: &gix::Repository, tip: ObjectId, base: ObjectId) -> Result<bool> {
    let Some(merge_base) = repo.merge_base(tip, base)? else {
        return Ok(false);
    };
    let tree = |commit: ObjectId| -> Result<ObjectId> {
        Ok(repo.find_commit(commit)?.tree_id()?.detach())
    };
    trees_present(
        repo,
        Some(tree(merge_base.detach())?),
        Some(tree(tip)?),
        Some(tree(base)?),
    )
}

type Entries = BTreeMap<BString, (EntryMode, ObjectId)>;

fn entries(repo: &gix::Repository, tree: Option<ObjectId>) -> Result<Entries> {
    let mut result = Entries::new();
    let Some(tree) = tree else {
        return Ok(result);
    };
    let tree = repo.find_tree(tree)?;
    for entry in tree.decode()?.entries {
        result.insert(
            entry.filename.to_owned(),
            (entry.mode, entry.oid.to_owned()),
        );
    }
    Ok(result)
}

/// Compares one directory level of the merge base, tip and base trees,
/// recursing only where the branch changed something.
fn trees_present(
    repo: &gix::Repository,
    merge_base: Option<ObjectId>,
    tip: Option<ObjectId>,
    base: Option<ObjectId>,
) -> Result<bool> {
    if merge_base == tip || tip == base {
        return Ok(true);
    }
    let (merge_base, tip, base) = (
        entries(repo, merge_base)?,
        entries(repo, tip)?,
        entries(repo, base)?,
    );
    let split = |entry: Option<&(EntryMode, ObjectId)>| match entry {
        Some((mode, oid)) if mode.is_tree() => (Some(*oid), None),
        Some(entry) => (None, Some(*entry)),
        None => (None, None),
    };
    for name in merge_base.keys().chain(tip.keys()) {
        let (old, new) = (merge_base.get(name), tip.get(name));
        if old == new {
            continue;
        }
        let (old_tree, old_blob) = split(old);
        let (new_tree, new_blob) = split(new);
        let (base_tree, base_blob) = split(base.get(name));
        if old_blob != new_blob && base_blob != new_blob {
            return Ok(false);
        }
        if old_tree != new_tree && !trees_present(repo, old_tree, new_tree, base_tree)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `tip` has commits that nothing in `roots` reaches.
pub fn has_unreachable(repo: &gix::Repository, tip: ObjectId, roots: &[ObjectId]) -> Result<bool> {
    if roots.contains(&tip) {
        return Ok(false);
    }
    let mut walk = repo
        .rev_walk([tip])
        .with_hidden(roots.iter().copied())
        .all()?;
    Ok(walk.next().transpose()?.is_some())
}
