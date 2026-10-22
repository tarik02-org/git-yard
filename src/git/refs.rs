//! Base branches, remotes, and branch name rules.

use anyhow::Result;

use crate::core::facts::BaseRef;
use crate::git::remote::Remote;

const CONVENTIONAL: [&str; 4] = ["main", "master", "trunk", "develop"];

pub fn remotes(repo: &gix::Repository) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = repo
        .remote_names()
        .into_iter()
        .map(|name| {
            let url = repo
                .find_remote(AsRef::<gix::bstr::BStr>::as_ref(&name))
                .ok()
                .and_then(|remote| {
                    remote
                        .url(gix::remote::Direction::Fetch)
                        .map(|url| url.to_bstring().to_string())
                });
            Remote {
                name: name.to_string(),
                url,
            }
        })
        .collect();
    remotes.sort_by_key(|remote| (remote.name != "origin", remote.name.clone()));
    remotes
}

/// The commit a ref points at, if it exists and peels to a commit.
pub fn commit_of(repo: &gix::Repository, name: &str) -> Option<gix::ObjectId> {
    let mut reference = repo.try_find_reference(name).ok()??;
    let id = reference.peel_to_id().ok()?;
    let commit = id.object().ok()?.peel_to_commit().ok()?;
    Some(commit.id)
}

/// The branches integration is measured against: the default branch of the
/// first remote that announces one (`origin` first), else a conventional
/// name, as a local branch and on that remote.
pub fn base_refs(repo: &gix::Repository) -> Vec<BaseRef> {
    let remotes = remotes(repo);
    let announced = remotes.iter().find_map(|remote| {
        let head = repo
            .try_find_reference(format!("refs/remotes/{}/HEAD", remote.name).as_str())
            .ok()??;
        let gix::refs::TargetRef::Symbolic(target) = head.target() else {
            return None;
        };
        let prefix = format!("refs/remotes/{}/", remote.name);
        let branch = target
            .as_bstr()
            .to_string()
            .strip_prefix(&prefix)?
            .to_owned();
        Some((Some(remote.name.clone()), branch))
    });
    let chosen = announced.or_else(|| {
        CONVENTIONAL.iter().find_map(|name| {
            let remote = remotes
                .iter()
                .find(|remote| {
                    commit_of(repo, &format!("refs/remotes/{}/{name}", remote.name)).is_some()
                })
                .map(|remote| remote.name.clone());
            let local = commit_of(repo, &format!("refs/heads/{name}")).is_some();
            (local || remote.is_some()).then(|| (remote, (*name).to_owned()))
        })
    });
    let Some((remote, branch)) = chosen else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut candidates = vec![(format!("refs/heads/{branch}"), branch.clone())];
    if let Some(remote) = remote {
        candidates.push((
            format!("refs/remotes/{remote}/{branch}"),
            format!("{remote}/{branch}"),
        ));
    }
    for (refname, name) in candidates {
        if let Some(oid) = commit_of(repo, &refname) {
            result.push(BaseRef {
                name,
                oid: oid.to_string(),
                branch: branch.clone(),
            });
        }
    }
    result
}

/// Whether `name` can be a new local branch.
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    let full = format!("refs/heads/{name}");
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name != "HEAD"
        && gix::validate::reference::name(full.as_str().into()).is_ok();
    if valid {
        Ok(())
    } else {
        Err(format!("invalid branch name: {name}"))
    }
}

/// Peels every reference of `repo` to a commit, skipping the rest.
pub fn all_ref_commits(repo: &gix::Repository, prefix: Option<&str>) -> Result<Vec<gix::ObjectId>> {
    let platform = repo.references()?;
    let iter = match prefix {
        Some(prefix) => platform.prefixed(prefix)?,
        None => platform.all()?,
    };
    let mut commits = Vec::new();
    for reference in iter {
        let Ok(mut reference) = reference else {
            continue;
        };
        let Ok(id) = reference.peel_to_id() else {
            continue;
        };
        if let Ok(commit) = id.object().and_then(|object| object.peel_to_commit()) {
            commits.push(commit.id);
        }
    }
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_follow_git_rules() {
        assert!(validate_branch_name("feature/x").is_ok());
        assert!(validate_branch_name("-x").is_err());
        assert!(validate_branch_name("HEAD").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("a b").is_err());
        assert!(validate_branch_name("").is_err());
    }
}
