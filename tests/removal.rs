//! The removal protocol against real repositories: what it removes, what
//! it refuses, and recovery from a crash at every step.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use common::{Env, candidate};
use git_yard::core::ids::{RepoKey, WorktreeKey};
use git_yard::core::policy::{Block, eligibility};
use git_yard::git::open::Registry;
use git_yard::removal::protocol::{Context, Injected, Step, no_probe, remove};
use git_yard::removal::reconcile::{Action, reconcile};
use git_yard::removal::request::{Outcome, Request};
use git_yard::store::journal::{Journal, Phase};
use git_yard::store::lock::RepoLocks;

struct Setup {
    env: Env,
    main: PathBuf,
    registry: Registry,
    journal: Journal,
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
            journal: Journal::open(&state.join("journal.jsonl")).unwrap(),
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

    fn remove_with(
        &self,
        request: &Request,
        probe: &(dyn Fn(Step) -> Option<Injected> + Sync),
    ) -> Option<Outcome> {
        remove(
            &Context {
                registry: &self.registry,
                journal: &self.journal,
                locks: &self.locks,
                cancelled: &|| false,
                on_removing: &|| {},
                probe,
            },
            request,
        )
    }

    fn remove(&self, request: &Request) -> Outcome {
        self.remove_with(request, &no_probe).unwrap()
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

    fn leftovers(&self) -> Vec<String> {
        fs::read_dir(&self.env.root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("git-yard-trash"))
            .collect()
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
            branch: Some("done".into()),
            warnings: vec![]
        }
    );
    assert!(!wt.exists());
    assert!(!s.registered("done"));
    assert!(s.branch_exists("done"));
    assert!(s.leftovers().is_empty());
    let (operations, _) = s.journal.operations().unwrap();
    let phases: Vec<&str> = operations[0]
        .entries
        .iter()
        .map(|e| e.phase.name())
        .collect();
    assert_eq!(
        phases,
        [
            "intent",
            "quarantined",
            "registration-removed",
            "deleted",
            "done"
        ]
    );
}

#[test]
fn changes_after_assessment_block_and_leave_everything_in_place() {
    let s = Setup::new();
    let edited = s.worktree("edited");
    let request = s.request(&edited);
    fs::write(edited.join("notes.txt"), "more work\n").unwrap();
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(edited.join("notes.txt").exists() && s.registered("edited"));

    // Ignored files are not status evidence; their ctime catches the edit.
    let ignored = s.worktree("ignored");
    let request = s.request(&ignored);
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(ignored.join("build.log"), "new output\n").unwrap();
    let outcome = s.remove(&request);
    assert!(
        matches!(&outcome, Outcome::Blocked { reason } if reason.contains("changed after assessment")),
        "{outcome:?}"
    );
    assert!(ignored.join("build.log").exists() && s.registered("ignored"));
    assert!(s.leftovers().is_empty());

    let committed = s.worktree("committed");
    let request = s.request(&committed);
    s.env.commit(&committed, "more", "x\n");
    assert!(matches!(s.remove(&request), Outcome::Blocked { .. }));
    assert!(committed.exists());
}

#[test]
fn nested_repositories_are_found_at_removal_and_put_back() {
    let s = Setup::new();
    let wt = s.worktree("nested");
    fs::create_dir_all(wt.join("vendor/lib")).unwrap();
    s.env.git(&wt.join("vendor/lib"), &["init", "-q"]);
    fs::write(s.env.root.join("project/.git/info/exclude"), "vendor/\n").unwrap();
    let request = s.request(&wt);
    let outcome = s.remove(&request);
    assert!(
        matches!(&outcome, Outcome::Blocked { reason } if reason.contains("nested repository")),
        "{outcome:?}"
    );
    assert!(wt.join("vendor/lib/.git").is_dir());
    assert!(s.leftovers().is_empty());
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
fn a_concurrent_prune_still_ends_in_removal() {
    let s = Setup::new();
    let wt = s.worktree("raced");
    let request = s.request(&wt);
    let admin = s.env.common_dir(&s.main).join("worktrees/project.raced");
    let probe = move |step| {
        if step == Step::GitRemove {
            fs::remove_dir_all(&admin).unwrap();
        }
        None
    };
    assert!(s.remove_with(&request, &probe).unwrap().is_removed());
    assert!(!wt.exists() && s.leftovers().is_empty());
}

#[test]
fn journal_failures_abort_before_and_warn_after_the_irreversible_step() {
    let s = Setup::new();
    let wt = s.worktree("early");
    let request = s.request(&wt);
    let outcome = s.remove_with(&request, &|step| {
        (step == Step::Intent).then_some(Injected::JournalError)
    });
    assert!(matches!(outcome, Some(Outcome::Blocked { .. })));
    assert!(wt.exists() && s.registered("early"));

    let outcome = s.remove_with(&request, &|step| {
        (step == Step::Quarantined).then_some(Injected::JournalError)
    });
    assert!(
        matches!(outcome, Some(Outcome::Blocked { .. })),
        "{outcome:?}"
    );
    assert!(wt.join("notes.txt").exists() && s.registered("early"));
    assert!(s.leftovers().is_empty());

    let outcome = s.remove_with(&request, &|step| {
        (step == Step::RegistrationRemoved).then_some(Injected::JournalError)
    });
    match outcome {
        Some(Outcome::Removed { warnings, .. }) => assert_eq!(warnings.len(), 1),
        other => panic!("{other:?}"),
    }
    assert!(!wt.exists());
}

#[test]
fn a_crash_at_any_step_is_recovered_without_losing_files() {
    for step in [
        Step::Intent,
        Step::Rename,
        Step::Quarantined,
        Step::Scan,
        Step::GitRemove,
        Step::RegistrationRemoved,
        Step::Delete,
        Step::Deleted,
        Step::Done,
    ] {
        let s = Setup::new();
        let wt = s.worktree("crash");
        let request = s.request(&wt);
        let crashed = s.remove_with(&request, &|at| (at == step).then_some(Injected::Crash));
        assert_eq!(crashed, None, "{step:?}");

        let (recoveries, _) = reconcile(&s.journal, &s.locks).unwrap();
        let action = recoveries.first().map(|recovery| recovery.action.clone());
        match step {
            Step::Intent => assert_eq!(action, None),
            Step::Rename => assert_eq!(action, Some(Action::Resolved)),
            Step::Quarantined | Step::Scan | Step::GitRemove => {
                assert_eq!(action, Some(Action::Restored), "{step:?}")
            }
            Step::RegistrationRemoved | Step::Delete => {
                assert!(
                    matches!(&action, Some(Action::Attention { quarantine: Some(path), .. }) if path.join("notes.txt").exists()),
                    "{step:?}: {action:?}"
                );
            }
            Step::Deleted | Step::Done => assert_eq!(action, Some(Action::Resolved), "{step:?}"),
        }
        let before_git = matches!(
            step,
            Step::Intent | Step::Rename | Step::Quarantined | Step::Scan | Step::GitRemove
        );
        assert_eq!(wt.join("notes.txt").exists(), before_git, "{step:?}");
        assert_eq!(s.registered("crash"), before_git, "{step:?}");

        // A second pass changes nothing and forgets what was settled.
        let (again, _) = reconcile(&s.journal, &s.locks).unwrap();
        let settled = !matches!(action, Some(Action::Attention { .. }));
        assert_eq!(again.is_empty(), settled, "{step:?}: {again:?}");
        if !settled {
            assert!(matches!(again[0].action, Action::Attention { .. }));
        }
    }
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
    let (recoveries, _) = reconcile(&s.journal, &s.locks).unwrap();
    assert!(recoveries.is_empty());
    let last = &s.journal.operations().unwrap().0[0];
    assert_eq!(last.last().phase, Phase::Done);
}
