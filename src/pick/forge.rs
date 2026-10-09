//! Open pull/merge requests from GitHub (`gh`) and GitLab (`glab`), best
//! effort: a missing or unauthenticated CLI only means no request data.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Entry, Project, Remote, Target};
use crate::model::Timestamp;

const LIMIT: &str = "200";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Forge {
    GitHub,
    GitLab,
}

impl Forge {
    /// Worktrunk's shortcut for checking out a request.
    pub fn shortcut(self, number: u64) -> String {
        match self {
            Forge::GitHub => format!("pr:{number}"),
            Forge::GitLab => format!("mr:{number}"),
        }
    }

    /// Prefix for local branches of requests: `pr-12`, `mr-12`.
    pub fn ref_kind(self) -> &'static str {
        match self {
            Forge::GitHub => "pr",
            Forge::GitLab => "mr",
        }
    }

    /// The ref under which the forge publishes a request's head commit.
    pub fn head_ref(self, number: u64) -> String {
        match self {
            Forge::GitHub => format!("refs/pull/{number}/head"),
            Forge::GitLab => format!("refs/merge-requests/{number}/head"),
        }
    }

    pub fn sigil(self) -> &'static str {
        match self {
            Forge::GitHub => "#",
            Forge::GitLab => "!",
        }
    }
}

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

/// The forge hosting the project, judged by remote URLs, `origin` first.
pub fn detect(project: &Project) -> Option<Forge> {
    forge_remote(project).map(|(forge, _)| forge)
}

/// The remote requests are fetched from: the one `detect` judged by.
pub fn remote_for(project: &Project) -> Option<&Remote> {
    forge_remote(project).map(|(_, remote)| remote)
}

fn forge_remote(project: &Project) -> Option<(Forge, &Remote)> {
    let mut remotes: Vec<&Remote> = project.remotes.iter().collect();
    remotes.sort_by_key(|remote| remote.name != "origin");
    remotes.into_iter().find_map(|remote| {
        let host = host(remote.url.as_deref()?)?;
        if host.contains("github") {
            Some((Forge::GitHub, remote))
        } else if host.contains("gitlab") {
            Some((Forge::GitLab, remote))
        } else {
            None
        }
    })
}

/// Host of `https://host/…`, `ssh://user@host:port/…` or `user@host:path`.
fn host(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let authority = rest.split(['/', ':']).next()?;
    let host = authority.rsplit('@').next()?;
    (!host.is_empty()).then(|| host.to_lowercase())
}

pub fn fetch(forge: Forge, dir: &Path) -> Result<Vec<Request>> {
    match forge {
        Forge::GitHub => github(dir),
        Forge::GitLab => gitlab(dir),
    }
}

fn github(dir: &Path) -> Result<Vec<Request>> {
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

    let mut command = Command::new("gh");
    command
        .args(["pr", "list", "--state", "open", "--limit", LIMIT, "--json"])
        .arg("number,title,author,headRefName,headRepositoryOwner,isCrossRepository,updatedAt")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    let output = super::engine::run_quietly(command, dir, TIMEOUT).context("gh pr list")?;
    let prs: Vec<Pr> = serde_json::from_slice(&output).context("decoding gh output")?;
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

fn gitlab(dir: &Path) -> Result<Vec<Request>> {
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

    let mut command = Command::new("glab");
    command
        .args(["mr", "list", "--per-page", LIMIT, "--output", "json"])
        .env("NO_COLOR", "1")
        .env("NO_PROMPT", "1");
    let output = super::engine::run_quietly(command, dir, TIMEOUT).context("glab mr list")?;
    let mrs: Vec<Mr> = serde_json::from_slice(&output).context("decoding glab output")?;
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

/// Whether `request` is about the branch `entry` shows. Same-repository
/// requests match any local or remote branch of that name; fork requests
/// only a remote whose URL names the fork owner.
pub fn matches(request: &Request, entry: &Entry, project: &Project) -> bool {
    if entry.target.branch() != Some(request.branch.as_str()) {
        return false;
    }
    if !request.cross {
        return true;
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

/// Parses RFC 3339 timestamps as both CLIs print them
/// (`2026-10-05T12:34:56Z`, with optional fraction and offset).
fn parse_time(text: &str) -> Option<Timestamp> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);

    let split = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(split);
    let clock = clock.split('.').next()?;
    let mut clock = clock.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);

    let offset = match zone.chars().next() {
        None | Some('Z' | 'z') => 0,
        Some(sign) => {
            let (hours, minutes) = zone[1..].split_once(':')?;
            let minutes = hours.parse::<i64>().ok()? * 60 + minutes.parse::<i64>().ok()?;
            if sign == '-' {
                -minutes * 60
            } else {
                minutes * 60
            }
        }
    };

    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_come_from_every_url_style() {
        assert_eq!(
            host("git@github.com:o/r.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host("https://gitlab.example.com/o/r").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(
            host("ssh://git@GitLab.corp:2222/o/r.git").as_deref(),
            Some("gitlab.corp")
        );
    }

    #[test]
    fn rfc3339_times_parse_with_fraction_and_offset() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-05T12:34:56Z"), Some(1_791_203_696));
        assert_eq!(parse_time("2026-10-05T12:34:56.789Z"), Some(1_791_203_696));
        assert_eq!(parse_time("2026-10-05T14:34:56+02:00"), Some(1_791_203_696));
        assert_eq!(parse_time("garbage"), None);
    }

    #[test]
    fn fork_requests_match_only_the_fork_remote() {
        let project = Project {
            common_dir: "/r/.git".into(),
            root: "/".into(),
            label: "r".into(),
            main_path: "/r".into(),
            remotes: vec![
                super::super::Remote {
                    name: "origin".into(),
                    url: Some("git@github.com:me/r.git".into()),
                },
                super::super::Remote {
                    name: "alice".into(),
                    url: Some("https://github.com/alice/r".into()),
                },
            ],
        };
        let request = Request {
            number: 1,
            title: "t".into(),
            author: "alice".into(),
            branch: "fix".into(),
            fork_owner: Some("Alice".into()),
            cross: true,
            updated_at: None,
        };
        let remote = |remote: &str| Entry {
            target: Target::Remote {
                remote: remote.into(),
                branch: "fix".into(),
            },
            time: None,
            subject: None,
            base: false,
        };
        assert!(matches(&request, &remote("alice"), &project));
        assert!(!matches(&request, &remote("origin"), &project));
    }
}
