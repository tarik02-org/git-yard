#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git_yard::fs::walk::FoundRepo;
use git_yard::git::inventory::{Inventory, inventory};
use git_yard::git::open::{GitContext, Registry};
use tempfile::TempDir;

pub const OLD: i64 = 1_600_000_000;

/// A scratch directory with git configured only by the environment set
/// here: no user or system config, fixed identities and dates.
pub struct Env {
    pub dir: TempDir,
    pub root: PathBuf,
    home: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("work");
        let home = base.join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        Env { dir, root, home }
    }

    pub fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{OLD} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{OLD} +0000"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
        command
    }

    /// Runs git and returns trimmed stdout; panics with stderr on failure.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.command(dir, args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub fn git_ok(&self, dir: &Path, args: &[&str]) -> bool {
        self.command(dir, args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// `<root>/<name>` with one commit on `main`.
    pub fn repo(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        self.git(&path, &["init", "-q", "-b", "main"]);
        self.commit(&path, "README", "hello\n");
        path
    }

    pub fn commit(&self, dir: &Path, file: &str, content: &str) -> String {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        self.git(dir, &["add", "--", file]);
        self.git(dir, &["commit", "-q", "-m", &format!("add {file}")]);
        self.git(dir, &["rev-parse", "HEAD"])
    }

    pub fn worktree(&self, main: &Path, name: &str, args: &[&str]) -> PathBuf {
        let path = main.with_file_name(format!(
            "{}.{name}",
            main.file_name().unwrap().to_string_lossy()
        ));
        let path_text = path.to_str().unwrap();
        let mut full = vec!["worktree", "add", "-q", path_text];
        full.extend_from_slice(args);
        self.git(main, &full);
        path
    }

    pub fn common_dir(&self, dir: &Path) -> PathBuf {
        let common = self.git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        fs::canonicalize(common).unwrap()
    }

    pub fn registry(&self) -> Registry {
        Registry::new(GitContext::isolated())
    }

    pub fn inventory(&self, registry: &Registry, dir: &Path) -> (gix::Repository, Inventory) {
        let common_dir = self.common_dir(dir);
        let repo = registry.open(&common_dir).unwrap();
        let found = FoundRepo {
            common_dir,
            root: self.root.clone(),
        };
        let inventory = inventory(&repo, &found, std::slice::from_ref(&self.root)).unwrap();
        (repo, inventory)
    }
}

/// One record of `git worktree list --porcelain -z`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: Option<String>,
    pub prunable: bool,
}

pub fn worktree_list(env: &Env, dir: &Path) -> Vec<Listed> {
    let output = env
        .command(dir, &["worktree", "list", "--porcelain", "-z"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut records = Vec::new();
    let mut current: Option<Listed> = None;
    for field in output.stdout.split(|byte| *byte == 0) {
        let field = String::from_utf8_lossy(field).into_owned();
        if field.is_empty() {
            records.extend(current.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field.as_str(), ""));
        let record = current.get_or_insert_with(Listed::default);
        match key {
            "worktree" => record.path = PathBuf::from(value),
            "HEAD" => record.head = Some(value.to_owned()),
            "branch" => {
                record.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                )
            }
            "bare" => record.bare = true,
            "detached" => record.detached = true,
            "locked" => record.locked = Some(value.to_owned()),
            "prunable" => record.prunable = true,
            _ => {}
        }
    }
    records.extend(current);
    records
}
