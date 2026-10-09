//! The built-in Git switcher against real repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use git_yard::config::{Config, Pick, Switcher};
use git_yard::discovery::FoundRepo;
use git_yard::pick::forge::{Forge, Request};
use git_yard::pick::launch::{self, Action};
use git_yard::pick::{Project, Target, list_project};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// An upstream with `feat/x` and a pull request ref, cloned to
/// `<root>/ns/charts`.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    main: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let up = dir.path().join("up");
        fs::create_dir_all(&up).unwrap();
        git(&up, &["init", "-q", "-b", "main"]);
        git(&up, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&up, &["branch", "feat/x"]);
        git(&up, &["commit", "-q", "--allow-empty", "-m", "pr head"]);
        git(&up, &["update-ref", "refs/pull/7/head", "HEAD"]);
        git(&up, &["reset", "-q", "--hard", "HEAD~"]);

        let root = dir.path().join("work");
        let main = root.join("ns/charts");
        fs::create_dir_all(&main).unwrap();
        git(
            dir.path(),
            &["clone", "-q", up.to_str().unwrap(), main.to_str().unwrap()],
        );
        Fixture {
            _dir: dir,
            root: fs::canonicalize(root).unwrap(),
            main: fs::canonicalize(main).unwrap(),
        }
    }

    fn project(&self) -> Project {
        let found = FoundRepo {
            common_dir: fs::canonicalize(self.main.join(".git")).unwrap(),
            root: self.root.clone(),
        };
        let mut project =
            list_project(&found, std::slice::from_ref(&self.root), &Config::default())
                .unwrap()
                .project;
        // Looks like GitHub to forge detection; fetches still use the clone's URL.
        project.remotes[0].url = Some("git@github.com:me/charts.git".into());
        project
    }
}

fn git_switcher() -> Pick {
    Pick {
        switcher: Switcher::Git,
        ..Pick::default()
    }
}

#[test]
fn remote_branch_becomes_a_tracking_worktree_beside_main() {
    let fixture = Fixture::new();
    let target = Target::Remote {
        remote: "origin".into(),
        branch: "feat/x".into(),
    };
    let switched = launch::switch(
        &git_switcher(),
        &fixture.project(),
        &target,
        None,
        &Action::Open,
    )
    .unwrap();

    let expected = fixture.root.join("ns/charts.feat-x");
    assert_eq!(switched.path, expected);
    assert_eq!(switched.branch.as_deref(), Some("feat/x"));
    assert_eq!(
        git(&expected, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feat/x"
    );
}

#[test]
fn new_branch_from_a_base_does_not_track_it() {
    let fixture = Fixture::new();
    let mut config = git_switcher();
    config.post_create = Some("printf '%s\\n' \"$GIT_YARD_BRANCH\" > created".into());
    let target = Target::Remote {
        remote: "origin".into(),
        branch: "main".into(),
    };
    let action = Action::Create {
        name: "topic$(touch${IFS}injected)".into(),
    };
    let switched = launch::switch(&config, &fixture.project(), &target, None, &action).unwrap();

    assert_eq!(
        git(&switched.path, &["branch", "--show-current"]),
        "topic$(touch${IFS}injected)"
    );
    let upstream = Command::new("git")
        .arg("-C")
        .arg(&switched.path)
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .output()
        .unwrap();
    assert!(!upstream.status.success());
    assert_eq!(
        fs::read_to_string(switched.path.join("created")).unwrap(),
        "topic$(touch${IFS}injected)\n"
    );
    assert!(!switched.path.join("injected").exists());
}

#[test]
fn request_is_fetched_from_the_forge_ref() {
    let fixture = Fixture::new();
    let request = Request {
        number: 7,
        title: "t".into(),
        author: "a".into(),
        branch: "fix-thing".into(),
        fork_owner: Some("alice".into()),
        cross: true,
        updated_at: None,
    };
    let target = Target::Request { number: 7 };
    git(&fixture.main, &["branch", "fix-thing"]);
    git(&fixture.main, &["branch", "pr-7"]);
    let before = git(&fixture.main, &["rev-parse", "pr-7"]);
    let collision = launch::switch(
        &git_switcher(),
        &fixture.project(),
        &target,
        Some(&(Forge::GitHub, request.clone())),
        &Action::Open,
    )
    .unwrap_err();
    assert!(collision.to_string().contains("already exists"));
    assert_eq!(git(&fixture.main, &["rev-parse", "pr-7"]), before);
    git(&fixture.main, &["branch", "-D", "pr-7"]);
    let switched = launch::switch(
        &git_switcher(),
        &fixture.project(),
        &target,
        Some(&(Forge::GitHub, request)),
        &Action::Open,
    )
    .unwrap();

    assert_eq!(switched.path, fixture.root.join("ns/charts.pr-7"));
    assert_eq!(
        git(&switched.path, &["log", "-1", "--format=%s"]),
        "pr head"
    );
}

#[test]
fn existing_paths_are_not_reused() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("ns/charts.feat-x")).unwrap();
    let target = Target::Remote {
        remote: "origin".into(),
        branch: "feat/x".into(),
    };
    let error = launch::switch(
        &git_switcher(),
        &fixture.project(),
        &target,
        None,
        &Action::Open,
    )
    .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
}
