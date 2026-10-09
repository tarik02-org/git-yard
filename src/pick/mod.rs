//! Targets the picker can open across every discovered project: worktrees,
//! local branches, remote branches and forge requests.

pub mod engine;
pub mod forge;
pub mod launch;
pub mod rank;
pub mod store;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use git2::{BranchType, Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::discovery::{self, FoundRepo};
use crate::model::{DirState, Kind, Timestamp};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub common_dir: PathBuf,
    /// Scan root the project was found under; labels are relative to it.
    pub root: PathBuf,
    pub label: String,
    /// Main worktree, or the repository itself when bare; `wt` runs here.
    pub main_path: PathBuf,
    pub remotes: Vec<Remote>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Target {
    /// `branch` is `None` for a detached HEAD.
    Worktree {
        path: PathBuf,
        branch: Option<String>,
    },
    Local {
        branch: String,
    },
    Remote {
        remote: String,
        branch: String,
    },
    /// An open pull/merge request whose branch is on none of the remotes.
    Request {
        number: u64,
    },
}

impl Target {
    /// The branch name the target refers to, without a remote prefix.
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
    /// Last activity: Git activity of a worktree, or the tip commit time.
    pub time: Option<Timestamp>,
    pub subject: Option<String>,
    /// A default or protected branch.
    pub base: bool,
}

/// Everything Git knows locally about one project's targets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub project: Project,
    pub entries: Vec<Entry>,
}

/// Reads a project's worktrees and branches. Duplicates are left out: a
/// local branch checked out in a worktree, a remote branch tracked by a
/// local branch.
pub fn list_project(
    found: &FoundRepo,
    roots: &[PathBuf],
    config: &Config,
) -> Result<Listing, git2::Error> {
    let scan = discovery::scan_repo(found, roots)?;
    let repo = Repository::open(&found.common_dir)?;
    let mut base: HashSet<String> = discovery::base_refs(&repo)
        .into_iter()
        .map(|base| base.branch)
        .collect();
    base.extend(config.protected_branches.iter().cloned());

    let remotes: Vec<Remote> = repo
        .remotes()?
        .iter()
        .filter_map(|name| name.ok().flatten())
        .map(|name| Remote {
            name: name.to_owned(),
            url: repo
                .find_remote(name)
                .ok()
                .and_then(|remote| remote.url().ok().map(str::to_owned)),
        })
        .collect();

    let main = &scan.seeds[0];
    let project = Project {
        common_dir: found.common_dir.clone(),
        root: found.root.clone(),
        label: main.repo_label.clone(),
        main_path: main.path.clone(),
        remotes,
    };

    let mut entries = Vec::new();
    let mut checked_out = HashSet::new();
    for seed in &scan.seeds {
        if seed.kind == Kind::BareRepository || seed.dir == DirState::Missing {
            continue;
        }
        let head = read_head(&seed.gitdir);
        let (branch, tip) = match &head {
            Some(Head::Branch(name)) => (
                Some(name.clone()),
                repo.refname_to_id(&format!("refs/heads/{name}")).ok(),
            ),
            Some(Head::Detached(oid)) => (None, Some(*oid)),
            None => (None, None),
        };
        let (commit_time, subject) = describe(&repo, tip);
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
    for item in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = item?;
        let Ok(Some(name)) = branch.name().map(|name| name.map(str::to_owned)) else {
            continue;
        };
        if let Ok(upstream) = branch.upstream()
            && let Ok(Some(upstream)) = upstream.name()
        {
            tracked.insert(upstream.to_owned());
        }
        if checked_out.contains(&name) {
            continue;
        }
        let (time, subject) = describe(&repo, branch.get().target());
        entries.push(Entry {
            base: base.contains(&name),
            target: Target::Local { branch: name },
            time,
            subject,
        });
    }

    for item in repo.branches(Some(BranchType::Remote))? {
        let (branch, _) = item?;
        // `origin/HEAD` is a pointer to another remote branch.
        if branch.get().symbolic_target_bytes().is_some() {
            continue;
        }
        let Ok(Some(name)) = branch.name().map(|name| name.map(str::to_owned)) else {
            continue;
        };
        if tracked.contains(&name) {
            continue;
        }
        let Some((remote, short)) = split_remote(&name, &project.remotes) else {
            continue;
        };
        let (time, subject) = describe(&repo, branch.get().target());
        entries.push(Entry {
            base: base.contains(short),
            target: Target::Remote {
                remote: remote.to_owned(),
                branch: short.to_owned(),
            },
            time,
            subject,
        });
    }

    Ok(Listing { project, entries })
}

enum Head {
    Branch(String),
    Detached(Oid),
}

fn read_head(gitdir: &Path) -> Option<Head> {
    let text = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let text = text.trim();
    match text.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(Head::Branch(branch.to_owned())),
        None => Oid::from_str(text).ok().map(Head::Detached),
    }
}

fn describe(repo: &Repository, oid: Option<Oid>) -> (Option<Timestamp>, Option<String>) {
    let Some(commit) = oid.and_then(|oid| repo.find_commit(oid).ok()) else {
        return (None, None);
    };
    (
        Some(commit.time().seconds()),
        commit.summary().ok().flatten().map(str::to_owned),
    )
}

fn newest(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Splits `remote/branch` using the configured remote names, which may
/// themselves contain slashes; the longest matching remote wins.
fn split_remote<'a>(name: &'a str, remotes: &'a [Remote]) -> Option<(&'a str, &'a str)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let rest = name.strip_prefix(remote.name.as_str())?.strip_prefix('/')?;
            Some((remote.name.as_str(), rest))
        })
        .max_by_key(|(remote, _)| remote.len())
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
