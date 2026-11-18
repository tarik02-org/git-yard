//! Open pull/merge requests from GitHub (`gh`) and GitLab (`glab`), always
//! asked about the project the chosen remote names, never whatever the CLI
//! infers from the directory. Best effort: a missing or unauthenticated CLI
//! only means no request data.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::ids::Timestamp;
use crate::git::process::Supervisor;
use crate::git::remote::{Forge, ForgeKind};

use super::listing::{Entry, Project, Target};

const LIMIT: &str = "200";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub branch: String,
    /// Owner of the head repository when it differs from the base.
    pub fork_owner: Option<String>,
    /// From another project; GitLab does not report the owner.
    pub cross: bool,
    pub updated_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Requests {
    pub forge: Forge,
    pub fetched_at: Timestamp,
    pub items: Vec<Request>,
}

impl ForgeKind {
    /// Worktrunk's shortcut for checking out a request.
    pub fn shortcut(self, number: u64) -> String {
        match self {
            ForgeKind::GitHub => format!("pr:{number}"),
            ForgeKind::GitLab => format!("mr:{number}"),
        }
    }

    /// Prefix for local branches of requests: `pr-12`, `mr-12`.
    pub fn ref_kind(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "pr",
            ForgeKind::GitLab => "mr",
        }
    }

    /// The ref under which the forge publishes a request's head commit.
    pub fn head_ref(self, number: u64) -> String {
        match self {
            ForgeKind::GitHub => format!("refs/pull/{number}/head"),
            ForgeKind::GitLab => format!("refs/merge-requests/{number}/head"),
        }
    }

    pub fn sigil(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "#",
            ForgeKind::GitLab => "!",
        }
    }
}

/// The command that lists the forge project's open requests.
pub fn list_command(forge: &Forge) -> Command {
    match forge.kind {
        ForgeKind::GitHub => {
            let mut command = Command::new("gh");
            command
                .args(["pr", "list", "--repo", &forge.slug()])
                .args(["--state", "open", "--limit", LIMIT, "--json"])
                .arg("number,title,author,headRefName,headRepositoryOwner,isCrossRepository,updatedAt")
                .env("GH_PROMPT_DISABLED", "1")
                .env("NO_COLOR", "1");
            command
        }
        ForgeKind::GitLab => {
            let mut command = Command::new("glab");
            command
                .args(["mr", "list", "--repo", &forge.path()])
                .args(["--per-page", LIMIT, "--output", "json"])
                .env("GITLAB_HOST", &forge.host)
                .env("NO_COLOR", "1")
                .env("GLAB_NO_PROMPT", "1");
            command
        }
    }
}

pub fn fetch(supervisor: &Supervisor, project: &Project, forge: &Forge) -> Result<Vec<Request>> {
    let output = supervisor
        .run(
            list_command(forge),
            &project.main_path,
            TIMEOUT,
            Some(&project.repo),
        )
        .with_context(|| match forge.kind {
            ForgeKind::GitHub => "gh pr list",
            ForgeKind::GitLab => "glab mr list",
        })?;
    match forge.kind {
        ForgeKind::GitHub => parse_github(&output),
        ForgeKind::GitLab => parse_gitlab(&output),
    }
}

fn parse_github(output: &[u8]) -> Result<Vec<Request>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        number: u64,
        title: String,
        author: Option<Login>,
        head_ref_name: String,
        head_repository_owner: Option<Login>,
        is_cross_repository: bool,
        updated_at: Option<String>,
    }
    #[derive(Deserialize)]
    struct Login {
        login: String,
    }
    let prs: Vec<Pr> = serde_json::from_slice(output).context("decoding gh output")?;
    Ok(prs
        .into_iter()
        .map(|pr| Request {
            number: pr.number,
            title: pr.title,
            author: pr.author.map(|author| author.login).unwrap_or_default(),
            branch: pr.head_ref_name,
            fork_owner: pr
                .head_repository_owner
                .filter(|_| pr.is_cross_repository)
                .map(|owner| owner.login),
            cross: pr.is_cross_repository,
            updated_at: pr.updated_at.as_deref().and_then(parse_time),
        })
        .collect())
}

fn parse_gitlab(output: &[u8]) -> Result<Vec<Request>> {
    #[derive(Deserialize)]
    struct Mr {
        iid: u64,
        title: String,
        author: Option<User>,
        source_branch: String,
        source_project_id: Option<u64>,
        target_project_id: Option<u64>,
        updated_at: Option<String>,
    }
    #[derive(Deserialize)]
    struct User {
        username: String,
    }
    let mrs: Vec<Mr> = serde_json::from_slice(output).context("decoding glab output")?;
    Ok(mrs
        .into_iter()
        .map(|mr| Request {
            number: mr.iid,
            title: mr.title,
            author: mr.author.map(|author| author.username).unwrap_or_default(),
            branch: mr.source_branch,
            fork_owner: None,
            cross: mr.source_project_id != mr.target_project_id,
            updated_at: mr.updated_at.as_deref().and_then(parse_time),
        })
        .collect())
}

/// Whether `request` is about the branch `entry` shows. Same-project
/// requests match a worktree or local branch of that name, or that branch
/// on the forge's remote; fork requests only a remote whose URL names the
/// fork owner.
pub fn matches(request: &Request, entry: &Entry, project: &Project, forge: &Forge) -> bool {
    if entry.target.branch() != Some(request.branch.as_str()) {
        return false;
    }
    if !request.cross {
        return match &entry.target {
            Target::Remote { remote, .. } => *remote == forge.remote,
            _ => true,
        };
    }
    let (Target::Remote { remote, .. }, Some(owner)) = (&entry.target, &request.fork_owner) else {
        return false;
    };
    project
        .remotes
        .iter()
        .find(|candidate| &candidate.name == remote)
        .and_then(|remote| remote.url.as_deref())
        .is_some_and(|url| {
            let url = url.to_lowercase();
            let owner = owner.to_lowercase();
            url.contains(&format!("/{owner}/")) || url.contains(&format!(":{owner}/"))
        })
}

/// RFC 3339 as both CLIs print it, with optional fraction and offset.
fn parse_time(text: &str) -> Option<Timestamp> {
    text.parse::<jiff::Timestamp>()
        .ok()
        .map(|time| time.as_second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::{RepoKey, YardPath};
    use crate::git::remote::{Remote, forge};

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn queries_are_pinned_to_the_chosen_remote() {
        let github = forge("upstream", "git@github.example.com:org/app.git").unwrap();
        let command = list_command(&github);
        assert_eq!(command.get_program(), "gh");
        let args = args_of(&command);
        let repo = args.iter().position(|arg| arg == "--repo").unwrap();
        assert_eq!(args[repo + 1], "github.example.com/org/app");

        let gitlab = forge("origin", "https://gitlab.corp/group/sub/app").unwrap();
        let command = list_command(&gitlab);
        let args = args_of(&command);
        let repo = args.iter().position(|arg| arg == "--repo").unwrap();
        assert_eq!(args[repo + 1], "group/sub/app");
        assert!(command.get_envs().any(|(key, value)| {
            key == "GITLAB_HOST" && value == Some(std::ffi::OsStr::new("gitlab.corp"))
        }));
    }

    #[test]
    fn rfc3339_times_parse_with_fraction_and_offset() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-05T12:34:56.789Z"), Some(1_791_203_696));
        assert_eq!(parse_time("2026-10-05T14:34:56+02:00"), Some(1_791_203_696));
        assert_eq!(parse_time("garbage"), None);
    }

    #[test]
    fn requests_match_only_the_forge_remote_or_the_fork() {
        let project = Project {
            repo: RepoKey::new("/r/.git"),
            label: "r".into(),
            main_path: YardPath::new("/r"),
            remotes: vec![
                Remote {
                    name: "origin".into(),
                    url: Some("git@github.com:me/r.git".into()),
                },
                Remote {
                    name: "alice".into(),
                    url: Some("https://github.com/alice/r".into()),
                },
            ],
        };
        let forge = forge("origin", "git@github.com:me/r.git").unwrap();
        let remote = |remote: &str| Entry {
            target: Target::Remote {
                remote: remote.into(),
                branch: "fix".into(),
            },
            time: None,
            subject: None,
            base: false,
        };
        let mut request = Request {
            number: 1,
            title: "t".into(),
            author: "alice".into(),
            branch: "fix".into(),
            fork_owner: Some("Alice".into()),
            cross: true,
            updated_at: None,
        };
        assert!(matches(&request, &remote("alice"), &project, &forge));
        assert!(!matches(&request, &remote("origin"), &project, &forge));
        request.cross = false;
        assert!(matches(&request, &remote("origin"), &project, &forge));
        assert!(!matches(&request, &remote("alice"), &project, &forge));
    }
}
