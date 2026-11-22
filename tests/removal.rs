//! The removal protocol against real repositories: what it removes and
//! what it refuses.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use common::{Env, candidate};
use git_yard::core::ids::{RepoKey, WorktreeKey};
use git_yard::core::policy::{Block, eligibility};
use git_yard::git::open::Registry;
use git_yard::removal::protocol::{Context, remove};
use git_yard::removal::request::{Outcome, Request};
use git_yard::store::lock::RepoLocks;

struct Setup {
    env: Env,
    main: PathBuf,
    registry: Registry,
    locks: RepoLocks,
}

impl Setup {
    fn new() -> Setup {
        let env = Env::new();
        let main = env.repo("project");
        env.commit(&main, ".gitignore", "*.log\n");
        let state = env.dir.path().join("state");
        Setup {
            registry: env.registry(),
            locks: RepoLocks::new(state.join("locks")),
            env,
            main,
        }
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.env.worktree(&self.main, name, &["-b", name]);
        fs::write(path.join("notes.txt"), "untracked\n").unwrap();
        fs::write(path.join("build.log"), "ignored\n").unwrap();
        path
    }

    fn request(&self, path: &Path) -> Request {
        let candidate = candidate(&self.env, &self.registry, &self.main, path);
        assert_eq!(eligibility(&candidate), Ok(()));
        Request::from_candidate(&candidate, 1).unwrap()
    }

    fn remove(&self, request: &Request) -> Outcome {
        remove(
            &Context {
                registry: &self.registry,
                locks: &self.locks,
                cancelled: &|| false,
                on_removing: &|| {},
            },
            request,
        )
    }

    fn registered(&self, name: &str) -> bool {
        self.env
            .common_dir(&self.main)
            .join("worktrees")
            .join(format!("project.{name}"))
            .exists()
    }

    fn branch_exists(&self, name: &str) -> bool {
        self.env.git_ok(
            &self.main,
            &["show-ref", "--verify", &format!("refs/heads/{name}")],
        )
    }
}

#[test]
fn removes_directory_and_registration_and_keeps_the_branch() {
    let s = Setup::new();
    let wt = s.worktree("done");
    let request = s.request(&wt);
    let outcome = s.remove(&request);
    assert_eq!(
        outcome,
        Outcome::Removed {
            branch: Some("done".into())
        }
    );
    assert!(!wt.exists());
    assert!(!s.registered("done"));
    assert!(s.branch_exists("done"));
}

#[test]
fn changes_after_assessment_block_and_leave_everything_in_place() {
    let s = Setup::new();
    let edited = s.worktree("edited");
    let request = s.request(&edited);
    fs::write(edited.join("notes.txt"), "more work\n").unwrap();
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(edited.join("notes.txt").exists() && s.registered("edited"));

    let committed = s.worktree("committed");
    let request = s.request(&committed);
    s.env.commit(&committed, "more", "x\n");
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(committed.exists());
}

#[test]
fn ignored_files_and_repositories_are_deleted_with_the_worktree() {
    let s = Setup::new();
    let wt = s.worktree("ignored");
    fs::create_dir_all(wt.join("vendor/lib")).unwrap();
    s.env.git(&wt.join("vendor/lib"), &["init", "-q"]);
    fs::write(s.env.root.join("project/.git/info/exclude"), "vendor/\n").unwrap();
    let request = s.request(&wt);
    fs::write(wt.join("build.log"), "new output\n").unwrap();
    assert!(s.remove(&request).is_removed());
    assert!(!wt.exists() && !s.registered("ignored"));
}

#[test]
fn worktrees_with_submodules_are_removed_unless_a_submodule_changed() {
    let s = Setup::new();
    let library = s.env.repo("library");
    let allow = ["-c", "protocol.file.allow=always"];
    s.env.git(
        &s.main,
        &[
            &allow[..],
            &["submodule", "add", "-q", library.to_str().unwrap(), "lib"],
        ]
        .concat(),
    );
    s.env.git(&s.main, &["commit", "-q", "-m", "submodule"]);
    let init = [&allow[..], &["submodule", "update", "-q", "--init"]].concat();

    let clean = s.worktree("clean");
    s.env.git(&clean, &init);
    let request = s.request(&clean);
    assert!(s.remove(&request).is_removed());
    assert!(!clean.exists() && !s.registered("clean"));

    let edited = s.worktree("edited");
    s.env.git(&edited, &init);
    let request = s.request(&edited);
    s.env.commit(&edited.join("lib"), "local", "work\n");
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(edited.join("lib/local").exists() && s.registered("edited"));
}

#[test]
fn missing_directories_only_lose_their_registration() {
    let s = Setup::new();
    let gone = s.worktree("gone");
    fs::remove_dir_all(&gone).unwrap();
    let request = s.request(&gone);
    assert!(s.remove(&request).is_removed());
    assert!(!s.registered("gone") && s.branch_exists("gone"));

    let staged = s.worktree("staged");
    fs::write(staged.join("new"), "x").unwrap();
    s.env.git(&staged, &["add", "new"]);
    fs::remove_dir_all(&staged).unwrap();
    let blocked = candidate(&s.env, &s.registry, &s.main, &staged);
    assert_eq!(eligibility(&blocked), Err(Block::StagedWithoutDirectory));

    let parent = s.env.root.join("volume");
    fs::create_dir(&parent).unwrap();
    let unmounted = parent.join("wt");
    s.env.git(
        &s.main,
        &[
            "worktree",
            "add",
            "-q",
            unmounted.to_str().unwrap(),
            "-b",
            "unmounted",
        ],
    );
    fs::remove_dir_all(&parent).unwrap();
    let blocked = candidate(&s.env, &s.registry, &s.main, &unmounted);
    assert_eq!(eligibility(&blocked), Err(Block::ParentMissing));
}

#[test]
fn a_failed_lookup_is_never_reported_as_removed() {
    let s = Setup::new();
    let wt = s.worktree("lookup");
    let mut request = s.request(&wt);
    request.key = WorktreeKey::linked(&RepoKey::new(s.env.common_dir(&s.main)), "nonexistent");
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(wt.exists());

    request.path = s.env.root.join("never-existed").into();
    assert_eq!(s.remove(&request), Outcome::AlreadyGone);
}

#[test]
fn the_repository_lock_excludes_other_processes() {
    let s = Setup::new();
    let wt = s.worktree("locked");
    let request = s.request(&wt);
    // A second lock table opens its own file descriptions, as another
    // process would.
    let other = RepoLocks::new(s.env.dir.path().join("state/locks"));
    let guard = other.lock(&request.key.repo()).unwrap();
    let finished = Mutex::new(None);
    std::thread::scope(|scope| {
        scope.spawn(|| *finished.lock().unwrap() = Some(s.remove(&request)));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(finished.lock().unwrap().is_none());
        assert!(wt.exists());
        drop(guard);
    });
    assert!(finished.into_inner().unwrap().unwrap().is_removed());
}
