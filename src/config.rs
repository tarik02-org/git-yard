use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const CONFIG_FILE: &str = ".git-yard.toml";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Scan roots, relative to the config file. Defaults to the directory the
    /// command runs in.
    pub roots: Vec<PathBuf>,
    /// How many directory levels below a root to search for repositories.
    /// The search never descends into a repository it found.
    pub max_depth: usize,
    /// Directory names never searched for repositories.
    pub skip_dirs: Vec<String>,
    /// A worktree counts as stale when both its last use and its last commit
    /// are older than this.
    pub stale_days: u32,
    /// Branches that never start checked and are ranked down.
    pub protected_branches: Vec<String>,
    pub weights: Weights,
    pub workers: Workers,
    pub pick: Pick,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pick {
    /// A project fetched more recently than this is not fetched again.
    pub fetch_minutes: u64,
    /// Cached pull/merge requests younger than this are not refreshed.
    pub request_minutes: u64,
    /// Concurrent fetches, and separately concurrent forge queries.
    pub jobs: usize,
    pub switcher: Switcher,
    /// Where the built-in Git switcher puts new worktrees. `{main}` is the
    /// main worktree path, `{branch}` the branch with `/` replaced by `-`.
    pub worktree_path: String,
    /// Shell command run in a new worktree, with GIT_YARD_PATH and
    /// GIT_YARD_BRANCH in its environment.
    pub post_create: Option<String>,
}

/// What creates and switches worktrees for the picker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Switcher {
    /// Worktrunk when installed, otherwise Git.
    #[default]
    Auto,
    /// `wt switch`, with Worktrunk's hooks and path template.
    Wt,
    /// `git worktree add` with `worktree_path` and `post_create`.
    Git,
}

impl Default for Pick {
    fn default() -> Self {
        Self {
            fetch_minutes: 10,
            request_minutes: 5,
            jobs: 4,
            switcher: Switcher::Auto,
            worktree_path: "{main}.{branch}".into(),
            post_create: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub inactivity: f64,
    pub commit_age: f64,
    pub size: f64,
    pub integration: f64,
    /// Multiplier applied to the score of protected branches.
    pub protected_factor: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workers {
    pub scan: usize,
    /// Worktrees measured concurrently.
    pub measure: usize,
    /// Threads reading directories for all measurements together.
    pub walk: usize,
    pub delete: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: 4,
            skip_dirs: [
                "node_modules",
                "target",
                "dist",
                "build",
                "vendor",
                "__pycache__",
                ".cache",
                ".direnv",
                ".venv",
                "venv",
                ".npm",
                ".pnpm-store",
                ".cargo",
                ".rustup",
                ".gradle",
                ".m2",
                ".Trash",
            ]
            .map(String::from)
            .to_vec(),
            stale_days: 14,
            protected_branches: [
                "main",
                "master",
                "develop",
                "dev",
                "trunk",
                "staging",
                "production",
            ]
            .map(String::from)
            .to_vec(),
            weights: Weights::default(),
            workers: Workers::default(),
            pick: Pick::default(),
        }
    }
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            inactivity: 0.45,
            commit_age: 0.2,
            size: 0.2,
            integration: 0.15,
            protected_factor: 0.25,
        }
    }
}

impl Default for Workers {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, usize::from);
        Self {
            scan: cpus.clamp(2, 8),
            measure: (cpus / 2).clamp(2, 6),
            walk: cpus.clamp(2, 16),
            delete: 2,
        }
    }
}

impl Config {
    /// Loads `explicit`, or the nearest `.git-yard.toml` at or above `cwd`,
    /// or the user's `~/.config/git-yard/config.toml`.
    /// Command-line paths replace configured roots; without either, `cwd` is
    /// the only root.
    pub fn load(cwd: &Path, explicit: Option<&Path>, paths: &[PathBuf]) -> Result<Self> {
        let file = match explicit {
            Some(path) => Some(path.to_path_buf()),
            None => cwd
                .ancestors()
                .map(|dir| dir.join(CONFIG_FILE))
                .find(|candidate| candidate.is_file())
                .or_else(|| {
                    dirs::config_dir()
                        .map(|dir| dir.join("git-yard/config.toml"))
                        .filter(|file| file.is_file())
                }),
        };

        let mut config = match &file {
            Some(file) => {
                let text = std::fs::read_to_string(file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let mut config: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
                let base = file.parent().unwrap_or(cwd);
                config.roots = config
                    .roots
                    .iter()
                    .map(|root| base.join(expand_home(root)))
                    .collect();
                config
            }
            None => Config::default(),
        };

        if !paths.is_empty() {
            config.roots = paths
                .iter()
                .map(|path| cwd.join(expand_home(path)))
                .collect();
        }
        if config.roots.is_empty() {
            config.roots.push(cwd.to_path_buf());
        }
        config.workers.scan = config.workers.scan.max(1);
        config.workers.measure = config.workers.measure.max(1);
        config.workers.walk = config.workers.walk.max(1);
        config.workers.delete = config.workers.delete.max(1);
        config.pick.jobs = config.pick.jobs.max(1);
        Ok(config)
    }

    pub fn is_protected(&self, branch: &str) -> bool {
        self.protected_branches.iter().any(|name| name == branch)
    }
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_config_upwards_and_resolves_roots_relative_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "roots = [\"projects\"]\nstale_days = 3\n",
        )
        .unwrap();

        let config = Config::load(&nested, None, &[]).unwrap();
        assert_eq!(config.roots, vec![dir.path().join("projects")]);
        assert_eq!(config.stale_days, 3);

        let overridden = Config::load(&nested, None, &[PathBuf::from("x")]).unwrap();
        assert_eq!(overridden.roots, vec![nested.join("x")]);
    }

    #[test]
    fn defaults_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path(), None, &[]).unwrap();
        assert_eq!(config.roots, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "stale_dayz = 3\n").unwrap();
        assert!(Config::load(dir.path(), None, &[]).is_err());
    }
}
