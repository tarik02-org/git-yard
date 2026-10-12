//! Remotes and the forge projects they point at.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

/// A forge project, pinned to the remote that names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forge {
    pub kind: ForgeKind,
    pub remote: String,
    pub url: String,
    pub host: String,
    /// Everything before the project name: a user, an organization, or a
    /// GitLab group path with subgroups.
    pub owner: String,
    pub name: String,
}

impl Forge {
    /// `HOST/OWNER/NAME`, as `gh --repo` takes it.
    pub fn slug(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    /// `OWNER/NAME`.
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// The forge a remote URL points at, judged by its host name.
pub fn forge(remote: &str, url: &str) -> Option<Forge> {
    let parsed = gix::url::parse(gix::bstr::BStr::new(url)).ok()?;
    let host = parsed.host()?.to_lowercase();
    let kind = if host.contains("github") {
        ForgeKind::GitHub
    } else if host.contains("gitlab") {
        ForgeKind::GitLab
    } else {
        return None;
    };
    let path = String::from_utf8(parsed.path.to_vec()).ok()?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.rsplit_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(Forge {
        kind,
        remote: remote.to_owned(),
        url: url.to_owned(),
        host,
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// The project's forge: `origin` first, then other remotes by name.
pub fn project_forge(remotes: &[Remote]) -> Option<Forge> {
    let mut ordered: Vec<&Remote> = remotes.iter().collect();
    ordered.sort_by_key(|remote| (remote.name != "origin", remote.name.clone()));
    ordered
        .into_iter()
        .find_map(|remote| forge(&remote.name, remote.url.as_deref()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_url_style() {
        let github = forge("origin", "git@github.com:o/r.git").unwrap();
        assert_eq!(github.kind, ForgeKind::GitHub);
        assert_eq!(github.slug(), "github.com/o/r");

        let gitlab = forge("up", "https://gitlab.example.com/group/sub/r").unwrap();
        assert_eq!(gitlab.kind, ForgeKind::GitLab);
        assert_eq!(gitlab.owner, "group/sub");
        assert_eq!(gitlab.host, "gitlab.example.com");

        let ssh = forge("x", "ssh://git@GitLab.corp:2222/o/r.git").unwrap();
        assert_eq!(ssh.host, "gitlab.corp");
        assert_eq!(forge("x", "/work/project"), None);
        assert_eq!(forge("x", "https://example.com/o/r"), None);
    }

    #[test]
    fn origin_wins_over_other_remotes() {
        let remotes = [
            Remote {
                name: "fork".into(),
                url: Some("git@github.com:me/r.git".into()),
            },
            Remote {
                name: "origin".into(),
                url: Some("git@github.com:org/r.git".into()),
            },
        ];
        assert_eq!(project_forge(&remotes).unwrap().owner, "org");
    }
}
