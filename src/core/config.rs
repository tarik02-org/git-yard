//! Configuration: the user's config file or `--config`, never a file found
//! in the scanned tree, since it can name commands to run.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use super::error::{Error, Result};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Scan roots, relative to the config file.
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

/// The user's config file, if the platform has a config directory.
pub fn user_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("git-yard/config.toml"))
}

impl Config {
    /// Loads `explicit`, or the user's config file when it exists.
    /// Command-line `paths` replace configured roots; without either,
    /// `default_root` is the only root. Every root must exist.
    pub fn load(
        cwd: &Path,
        explicit: Option<&Path>,
        paths: &[PathBuf],
        default_root: impl FnOnce() -> PathBuf,
    ) -> Result<Self> {
        let file = match explicit {
            Some(path) => Some(cwd.join(path)),
            None => user_config_file().filter(|file| file.is_file()),
        };
        let mut config = match &file {
            Some(file) => {
                let text = std::fs::read_to_string(file).map_err(|source| Error::Io {
                    context: format!("reading {}", file.display()),
                    source,
                })?;
                let mut config: Config = toml::from_str(&text).map_err(|error| Error::Config {
                    file: file.clone(),
                    message: error.to_string(),
                })?;
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
            config.roots.push(default_root());
        }
        for root in &config.roots {
            if !root.is_dir() {
                return Err(Error::Usage(format!(
                    "scan root {} does not exist or is not a directory",
                    root.display()
                )));
            }
        }
        config.validate().map_err(|message| Error::Config {
            file: file.clone().unwrap_or_default(),
            message,
        })?;
        config.workers.scan = config.workers.scan.max(1);
        config.workers.measure = config.workers.measure.max(1);
        config.workers.walk = config.workers.walk.max(1);
        config.workers.delete = config.workers.delete.max(1);
        config.pick.jobs = config.pick.jobs.max(1);
        Ok(config)
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        let w = &self.weights;
        let parts = [
            ("inactivity", w.inactivity),
            ("commit_age", w.commit_age),
            ("size", w.size),
            ("integration", w.integration),
            ("protected_factor", w.protected_factor),
        ];
        for (name, value) in parts {
            if !value.is_finite() || value < 0.0 {
                return Err(format!(
                    "weights.{name} must be a finite number ≥ 0, got {value}"
                ));
            }
        }
        if parts[..4].iter().all(|(_, value)| *value == 0.0) {
            return Err("at least one rating weight must be above 0".into());
        }
        validate_worktree_path(&self.pick.worktree_path)
    }

    pub fn is_protected(&self, branch: &str) -> bool {
        self.protected_branches.iter().any(|name| name == branch)
    }
}

/// New worktrees must land in a directory the template names, never above
/// it: no `..` components, and the branch only in the last component.
fn validate_worktree_path(template: &str) -> std::result::Result<(), String> {
    let path = Path::new(template);
    let invalid = |reason: &str| Err(format!("pick.worktree_path {template:?}: {reason}"));
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return invalid("must not contain `..`");
    }
    let Some(last) = path.file_name().and_then(|name| name.to_str()) else {
        return invalid("must end in a file name");
    };
    if !last.contains("{branch}") {
        return invalid("the last component must contain {branch}");
    }
    let parent = path.parent().map(|parent| parent.to_string_lossy());
    if parent.is_some_and(|parent| parent.contains("{branch}")) {
        return invalid("{branch} may only appear in the last component");
    }
    Ok(())
}

pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(dir: &Path, text: &str) -> Result<Config> {
        let file = dir.join("config.toml");
        std::fs::write(&file, text).unwrap();
        Config::load(dir, Some(&file), &[], || dir.to_path_buf())
    }

    #[test]
    fn roots_resolve_relative_to_the_config_file_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("projects")).unwrap();
        let config = load(dir.path(), "roots = [\"projects\"]\nstale_days = 3\n").unwrap();
        assert_eq!(config.roots, vec![dir.path().join("projects")]);
        assert_eq!(config.stale_days, 3);

        let error = load(dir.path(), "roots = [\"missing\"]\n").unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn rejects_unknown_keys_bad_weights_and_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), "stale_dayz = 3\n").is_err());
        assert!(load(dir.path(), "[weights]\nsize = nan\n").is_err());
        assert!(load(dir.path(), "[weights]\nsize = -1\n").is_err());
        assert!(
            load(
                dir.path(),
                "[weights]\ninactivity = 0\ncommit_age = 0\nsize = 0\nintegration = 0\n"
            )
            .is_err()
        );
        assert!(load(dir.path(), "[pick]\nworktree_path = \"../{branch}\"\n").is_err());
        assert!(load(dir.path(), "[pick]\nworktree_path = \"{branch}/x\"\n").is_err());
        assert!(
            load(
                dir.path(),
                "[pick]\nworktree_path = \"{main}.wt/{branch}\"\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn defaults_to_the_given_root() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path(), None, &[], || dir.path().to_path_buf());
        // A user config on the machine running tests may set roots; only
        // check the fallback when there is none.
        if user_config_file().is_none_or(|file| !file.is_file()) {
            assert_eq!(config.unwrap().roots, vec![dir.path().to_path_buf()]);
        }
    }
}
