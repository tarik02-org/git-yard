//! What the picker can open in one project: worktrees, local branches and
//! remote branches. Requests come from `forge`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::candidate::Kind;
use crate::core::config::Config;
use crate::core::facts::Head;
use crate::core::ids::{RepoKey, Timestamp, YardPath};
use crate::fs::walk::FoundRepo;
use crate::git::remote::Remote;
use crate::git::{head, inventory, refs};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub repo: RepoKey,
    pub label: String,
    /// Main worktree, or the repository itself when bare; `wt` runs here.
    pub main_path: YardPath,
    pub remotes: Vec<Remote>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Target {
    /// `branch` is `None` for a detached HEAD.
    Worktree {
        path: YardPath,
        branch: Option<String>,
    },
    Local {
        branch: String,
    },
    Remote {
        remote: String,
        branch: String,
    },
    /// An open request whose branch matches nothing listed.
    Request {
        number: u64,
    },
}

impl Target {
    /// The branch name, without a remote prefix.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Target::Worktree { branch, .. } => branch.as_deref(),
            Target::Local { branch } | Target::Remote { branch, .. } => Some(branch),
            Target::Request { .. } => None,
        }
    }

    /// Stable identity within a project, for pick history.
    pub fn key(&self) -> String {
        match self {
            Target::Worktree { path, .. } => format!("wt:{}", path.display()),
            Target::Local { branch } => format!("local:{branch}"),
            Target::Remote { remote, branch } => format!("remote:{remote}/{branch}"),
            Target::Request { number } => format!("request:{number}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub target: Target,
    /// Git activity of a worktree, or the tip commit time.
    pub time: Option<Timestamp>,
    pub subject: Option<String>,
    /// A default or protected branch.
    pub base: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub project: Project,
    pub entries: Vec<Entry>,
}

/// Reads one project. Duplicates are left out: a local branch checked out
/// in a worktree, a remote branch tracked by a local branch.
pub fn list_project(
    repo: &gix::Repository,
    found: &FoundRepo,
    roots: &[PathBuf],
    config: &Config,
) -> Result<Listing> {
    let inventory = inventory::inventory(repo, found, roots)?;
    let mut base: HashSet<String> = refs::base_refs(repo)
        .into_iter()
        .map(|base| base.branch)
        .collect();
    base.extend(config.protected_branches.iter().cloned());
    let project = Project {
        repo: inventory.repo.clone(),
        label: inventory.label.clone(),
        main_path: YardPath::new(&inventory.main_path),
        remotes: refs::remotes(repo),
    };

    let mut entries = Vec::new();
    let mut checked_out = HashSet::new();
    for seed in &inventory.seeds {
        if seed.kind == Kind::BareRepository || !seed.dir.is_present() {
            continue;
        }
        let Ok(worktree) = inventory::worktree_repo(repo, seed) else {
            continue;
        };
        let head = head::read_head(&worktree).ok();
        let branch = head.as_ref().and_then(Head::branch).map(str::to_owned);
        let tip = head.as_ref().and_then(Head::oid).map(str::to_owned);
        let (commit_time, subject) = describe(repo, tip.as_deref());
        if let Some(branch) = &branch {
            checked_out.insert(branch.clone());
        }
        entries.push(Entry {
            base: branch.as_ref().is_some_and(|branch| base.contains(branch)),
            target: Target::Worktree {
                path: seed.path.clone(),
                branch,
            },
            time: newest(seed.git_activity, commit_time),
            subject,
        });
    }

    let mut tracked = HashSet::new();
    for reference in repo.references()?.local_branches()? {
        let Ok(reference) = reference else { continue };
        let name = reference.name().shorten().to_string();
        if let Some(Ok(upstream)) =
            repo.branch_remote_tracking_ref_name(reference.name(), gix::remote::Direction::Fetch)
        {
            tracked.insert(upstream.shorten().to_string());
        }
        if checked_out.contains(&name) {
            continue;
        }
        let tip = refs::commit_of(repo, &format!("refs/heads/{name}")).map(|oid| oid.to_string());
        let (time, subject) = describe(repo, tip.as_deref());
        entries.push(Entry {
            base: base.contains(&name),
            target: Target::Local { branch: name },
            time,
            subject,
        });
    }

    for reference in repo.references()?.remote_branches()? {
        let Ok(reference) = reference else { continue };
        // `origin/HEAD` points at another remote branch.
        if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
            continue;
        }
        let name = reference.name().shorten().to_string();
        if tracked.contains(&name) {
            continue;
        }
        let Some((remote, branch)) = split_remote(&name, &project.remotes) else {
            continue;
        };
        let full = reference.name().as_bstr().to_string();
        let tip = refs::commit_of(repo, &full).map(|oid| oid.to_string());
        let (time, subject) = describe(repo, tip.as_deref());
        entries.push(Entry {
            base: base.contains(branch),
            target: Target::Remote {
                remote: remote.to_owned(),
                branch: branch.to_owned(),
            },
            time,
            subject,
        });
    }
    Ok(Listing { project, entries })
}

fn describe(repo: &gix::Repository, oid: Option<&str>) -> (Option<Timestamp>, Option<String>) {
    let commit = oid
        .and_then(|oid| gix::ObjectId::from_hex(oid.as_bytes()).ok())
        .and_then(|oid| repo.find_commit(oid).ok());
    let Some(commit) = commit else {
        return (None, None);
    };
    let time = commit.time().ok().map(|time| time.seconds);
    let subject = commit
        .message()
        .ok()
        .map(|message| message.summary().to_string());
    (time, subject)
}

fn newest(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Splits `remote/branch` on the configured remote names, which may contain
/// slashes themselves; the longest matching remote wins.
pub fn split_remote<'a>(name: &'a str, remotes: &'a [Remote]) -> Option<(&'a str, &'a str)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let rest = name.strip_prefix(remote.name.as_str())?.strip_prefix('/')?;
            Some((remote.name.as_str(), rest))
        })
        .max_by_key(|(remote, _)| remote.len())
}

/// The project a worktree path belongs to, for pick history keys.
pub fn history_key(repo: &Path, target: &Target) -> String {
    format!("{}::{}", repo.display(), target.key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_with_slashes_split_on_the_longest_remote() {
        let remotes = ["origin", "team/fork"].map(|name| Remote {
            name: name.into(),
            url: None,
        });
        assert_eq!(
            split_remote("team/fork/feat/x", &remotes),
            Some(("team/fork", "feat/x"))
        );
        assert_eq!(
            split_remote("origin/main", &remotes),
            Some(("origin", "main"))
        );
        assert_eq!(split_remote("other/main", &remotes), None);
    }
}
