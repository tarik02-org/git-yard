//! Switching to picked targets with the built-in Git switcher.

mod common;

use std::fs;
use std::path::PathBuf;

use common::Env;
use git_yard::core::config::{Pick, Switcher};
use git_yard::core::ids::{RepoKey, YardPath};
use git_yard::git::remote::{Forge, ForgeKind, Remote};
use git_yard::pick::forge::Request;
use git_yard::pick::launch::{self, Action};
use git_yard::pick::listing::{Project, Target};

struct Fixture {
    env: Env,
    main: PathBuf,
    origin: PathBuf,
}

impl Fixture {
    /// `<root>/ns/charts` cloned from an origin with `feat/x` and a request
    /// head at `refs/pull/7/head`.
    fn new() -> Fixture {
        let env = Env::new();
        let origin = env.repo("origin");
        env.git(&origin, &["branch", "feat/x"]);
        env.git(&origin, &["switch", "-q", "-c", "pr"]);
        env.commit(&origin, "fix", "fixed\n");
        env.git(&origin, &["commit", "-q", "--amend", "-m", "pr head"]);
        env.git(&origin, &["update-ref", "refs/pull/7/head", "HEAD"]);
        env.git(&origin, &["switch", "-q", "main"]);
        let main = env.root.join("ns/charts");
        fs::create_dir_all(main.parent().unwrap()).unwrap();
        env.git(
            &env.root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                main.to_str().unwrap(),
            ],
        );
        Fixture { env, main, origin }
    }

    fn project(&self) -> Project {
        Project {
            repo: RepoKey::new(self.env.common_dir(&self.main)),
            label: "ns/charts".into(),
            main_path: YardPath::new(&self.main),
            remotes: vec![Remote {
                name: "origin".into(),
                url: Some(self.origin.display().to_string()),
            }],
        }
    }

    fn forge(&self) -> Forge {
        Forge {
            kind: ForgeKind::GitHub,
            remote: "origin".into(),
            url: self.origin.display().to_string(),
            host: "github.com".into(),
            owner: "o".into(),
            name: "charts".into(),
        }
    }

    fn git(&self, args: &[&str]) -> String {
        self.env.git(&self.main, args)
    }
}

fn git_switcher() -> Pick {
    Pick {
        switcher: Switcher::Git,
        ..Pick::default()
    }
}

fn remote(branch: &str) -> Target {
    Target::Remote {
        remote: "origin".into(),
        branch: branch.into(),
    }
}

#[test]
fn remote_branch_becomes_a_tracking_worktree_beside_main() {
    let f = Fixture::new();
    let switched = launch::switch(
        &git_switcher(),
        &f.project(),
        &remote("feat/x"),
        None,
        &Action::Open,
    )
    .unwrap();
    assert_eq!(switched.path, f.env.root.join("ns/charts.feat-x"));
    assert_eq!(switched.branch.as_deref(), Some("feat/x"));
    assert_eq!(
        f.env.git(
            &switched.path,
            &["rev-parse", "--abbrev-ref", "@{upstream}"]
        ),
        "origin/feat/x"
    );
}

#[test]
fn a_remote_branch_shadowed_by_a_local_one_gets_its_own_name() {
    let f = Fixture::new();
    f.git(&["branch", "--no-track", "feat/x", "main"]);
    let switched = launch::switch(
        &git_switcher(),
        &f.project(),
        &remote("feat/x"),
        None,
        &Action::Open,
    )
    .unwrap();
    assert_eq!(switched.branch.as_deref(), Some("origin-feat/x"));
    assert_eq!(
        f.env.git(
            &switched.path,
            &["rev-parse", "--abbrev-ref", "@{upstream}"]
        ),
        "origin/feat/x"
    );
}

#[test]
fn new_branches_do_not_track_and_names_never_reach_a_shell() {
    let f = Fixture::new();
    let mut config = git_switcher();
    config.post_create = Some("printf '%s\\n' \"$GIT_YARD_BRANCH\" > created".into());
    let action = Action::Create {
        name: "topic$(touch${IFS}injected)".into(),
    };
    let switched = launch::switch(&config, &f.project(), &remote("main"), None, &action).unwrap();
    assert_eq!(
        f.env.git(&switched.path, &["branch", "--show-current"]),
        "topic$(touch${IFS}injected)"
    );
    assert!(!f.env.git_ok(
        &switched.path,
        &["rev-parse", "--abbrev-ref", "@{upstream}"]
    ));
    assert_eq!(
        fs::read_to_string(switched.path.join("created")).unwrap(),
        "topic$(touch${IFS}injected)\n"
    );
    assert!(!switched.path.join("injected").exists());
}

#[test]
fn requests_are_fetched_from_the_forge_remote_without_touching_branches() {
    let f = Fixture::new();
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
    let pinned = Some((f.forge(), request));
    f.git(&["branch", "fix-thing"]);
    f.git(&["branch", "pr-7"]);
    let before = f.git(&["rev-parse", "pr-7"]);
    let collision = launch::switch(
        &git_switcher(),
        &f.project(),
        &target,
        pinned.as_ref(),
        &Action::Open,
    )
    .unwrap_err();
    assert!(collision.to_string().contains("already exists"));
    assert_eq!(f.git(&["rev-parse", "pr-7"]), before);

    f.git(&["branch", "-D", "pr-7"]);
    let switched = launch::switch(
        &git_switcher(),
        &f.project(),
        &target,
        pinned.as_ref(),
        &Action::Open,
    )
    .unwrap();
    assert_eq!(switched.path, f.env.root.join("ns/charts.pr-7"));
    assert_eq!(
        f.env.git(&switched.path, &["log", "-1", "--format=%s"]),
        "pr head"
    );
    assert_eq!(f.git(&["for-each-ref", "refs/git-yard"]), "");
}

#[test]
fn existing_paths_are_not_reused() {
    let f = Fixture::new();
    fs::create_dir_all(f.env.root.join("ns/charts.feat-x")).unwrap();
    let error = launch::switch(
        &git_switcher(),
        &f.project(),
        &remote("feat/x"),
        None,
        &Action::Open,
    )
    .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
}
