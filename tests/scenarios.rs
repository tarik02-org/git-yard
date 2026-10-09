mod common;

use std::fs;
use std::time::Duration;

use common::{Fixture, OLD, row, scan, write};
use git_yard::engine::{CancelResult, DeleteState, Event};
use git_yard::model::{Integration, Obs};
use git_yard::policy::Block;
use git_yard::remove::{self, Outcome, Request, Step};
use git_yard::store::{JournalEntry, Phase};

#[test]
fn classifies_integration_dirty_locked_detached_and_missing() {
    let f = Fixture::new();
    let base = f.tip("main");

    let merged = f.commit("refs/heads/merged", Some(base), &[("a", "1\n")], OLD);
    f.commit("refs/heads/main", Some(merged), &[("b", "2\n")], OLD);

    f.commit("refs/heads/squashed", Some(base), &[("c", "3\n")], OLD);
    f.commit("refs/heads/main", None, &[("c", "3\n")], OLD);

    f.commit("refs/heads/unmerged", Some(base), &[("d", "4\n")], OLD);

    f.branch("dirty", merged);
    f.branch("locked", merged);
    f.branch("gone", merged);
    f.branch("detached-tmp", base);

    f.worktree("merged", "merged", None);
    f.worktree("squashed", "squashed", None);
    f.worktree("unmerged", "unmerged", None);
    let dirty = f.worktree("dirty", "dirty", None);
    write(&dirty.join("scratch.txt"), "local\n");
    f.worktree("locked", "locked", None);
    f.repo
        .find_worktree("locked")
        .unwrap()
        .lock(Some("on a USB disk"))
        .unwrap();
    let gone = f.worktree("gone", "gone", None);
    fs::remove_dir_all(&gone).unwrap();

    // Detached at a commit no reference reaches.
    let orphan = f.commit("", Some(base), &[("e", "5\n")], OLD);
    f.worktree("detached", "detached-tmp", None);
    write(
        &f.common.join("worktrees/detached/HEAD"),
        &format!("{orphan}\n"),
    );

    let (model, _engine, _events) = scan(f.config(), f.journal());
    let integration = |name: &str| {
        row(&model, &f.id(name))
            .candidate
            .git
            .complete()
            .unwrap()
            .integration
            .clone()
    };

    assert!(matches!(integration("merged"), Integration::Merged { .. }));
    assert!(matches!(
        integration("squashed"),
        Integration::ChangesPresent { .. }
    ));
    assert!(matches!(
        integration("unmerged"),
        Integration::NotIntegrated { ahead: 1, .. }
    ));

    let merged_row = row(&model, &f.id("merged"));
    assert_eq!(merged_row.eligibility, Ok(()));
    assert!(
        merged_row.recommendation.checked,
        "{:?}",
        merged_row.recommendation
    );
    assert!(row(&model, &f.id("squashed")).recommendation.checked);

    // Dirty and unmerged worktrees are removable but start unchecked.
    for name in ["dirty", "unmerged"] {
        let candidate = row(&model, &f.id(name));
        assert_eq!(candidate.eligibility, Ok(()), "{name}");
        assert!(!candidate.recommendation.checked, "{name}");
    }

    assert_eq!(
        row(&model, &f.id("locked")).eligibility,
        Err(Block::Locked(Some("on a USB disk".into())))
    );
    assert_eq!(
        row(&model, &f.id("detached")).eligibility,
        Err(Block::UnreachableCommits)
    );

    // A registration whose directory is gone stays an eligible candidate:
    // removing it only prunes the registration.
    let gone_row = row(&model, &f.id("gone"));
    assert_eq!(gone_row.eligibility, Ok(()));
    assert!(matches!(gone_row.candidate.usage, Obs::Failed { .. }));

    let main = model
        .rows
        .values()
        .find(|row| row.candidate.seed.path == f.main_path())
        .unwrap();
    assert_eq!(main.eligibility, Err(Block::MainWorktree));
}

#[test]
fn recent_activity_keeps_a_merged_worktree_unchecked() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    f.worktree("feature", "feature", None);
    let mut config = f.config();
    config.stale_days = 14;

    let (model, _engine, _events) = scan(config, f.journal());
    let feature = row(&model, &f.id("feature"));
    assert_eq!(feature.eligibility, Ok(()));
    assert!(!feature.recommendation.checked);
    assert!(
        feature
            .recommendation
            .reasons
            .iter()
            .any(|reason| reason.starts_with("used")),
        "{:?}",
        feature.recommendation.reasons
    );
}

#[test]
fn aliases_overlapping_roots_and_outside_worktrees_produce_one_row_each() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    let outside = f.dir.path().join("elsewhere/feature");
    let outside = f.worktree("feature", "feature", Some(outside));
    let alias = f.dir.path().join("alias");
    std::os::unix::fs::symlink(&f.root, &alias).unwrap();

    let mut config = f.config();
    config.roots = vec![f.root.clone(), f.root.join("ns"), alias];
    let (model, _engine, _events) = scan(config, f.journal());

    assert_eq!(model.repos, 1);
    assert_eq!(
        model.rows.len(),
        2,
        "{:?}",
        model.rows.keys().collect::<Vec<_>>()
    );
    assert_eq!(row(&model, &f.id("feature")).candidate.seed.path, outside);
    assert_eq!(
        row(&model, &f.id("feature")).candidate.seed.repo_label,
        "ns/project"
    );
}

#[test]
fn duplicate_registrations_of_one_directory_are_conflicts() {
    let f = Fixture::new();
    f.branch("a", f.tip("main"));
    let path = f.worktree("dup-a", "a", None);
    // A second registration claiming the same directory.
    let admin = f.common.join("worktrees");
    fs::create_dir_all(admin.join("dup-b")).unwrap();
    for file in ["HEAD", "commondir", "gitdir"] {
        fs::copy(
            admin.join("dup-a").join(file),
            admin.join("dup-b").join(file),
        )
        .unwrap();
    }

    let (model, _engine, _events) = scan(f.config(), f.journal());
    for name in ["dup-a", "dup-b"] {
        let candidate = row(&model, &f.id(name));
        assert!(
            matches!(candidate.eligibility, Err(Block::Conflict(_))),
            "{name}: {:?}",
            candidate.eligibility
        );
    }
    assert!(path.exists());
}

fn request(f: &Fixture, model: &git_yard::state::Model, name: &str) -> Request {
    Request::from_candidate(&row(model, &f.id(name)).candidate).unwrap()
}

#[test]
fn removes_directory_and_registration_and_keeps_the_branch() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    let path = f.worktree("feature", "feature", None);
    // Ignored-style build output does not block removal.
    fs::create_dir_all(path.join("target/debug")).unwrap();
    write(&path.join("target/debug/out"), "bin");
    write(&path.join(".gitignore"), "target/\n");
    let (model, _engine, _events) = scan(f.config(), f.journal());
    let request = request(&f, &model, "feature");
    // Only .gitignore counts as untracked; ignored output does not.
    assert_eq!(request.dirty.unwrap().untracked, 1);

    let journal = f.journal();
    let outcome = remove::run(&request, &journal, || false, || {});
    let Outcome::Removed { report } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(report.directory, Step::Done);
    assert_eq!(report.registration, Step::Done);
    assert_eq!(report.branch, Step::Retained);
    assert!(!path.exists());
    assert!(f.repo.find_worktree("feature").is_err());
    assert!(
        f.repo
            .find_branch("feature", git2::BranchType::Local)
            .is_ok()
    );

    let phases: Vec<Phase> = journal
        .entries()
        .unwrap()
        .into_iter()
        .map(|e| e.phase)
        .collect();
    assert_eq!(
        phases,
        [
            Phase::Intent,
            Phase::DirectoryRemoved,
            Phase::RegistrationPruned,
            Phase::Done
        ]
    );
}

#[test]
fn changed_evidence_blocks_removal() {
    let f = Fixture::new();
    f.branch("moved", f.tip("main"));
    f.branch("edited", f.tip("main"));
    f.branch("locked", f.tip("main"));
    let moved = f.worktree("moved", "moved", None);
    let edited = f.worktree("edited", "edited", None);
    let locked = f.worktree("locked", "locked", None);
    let (model, _engine, _events) = scan(f.config(), f.journal());
    let journal = f.journal();

    f.commit("refs/heads/moved", None, &[("new", "x\n")], OLD);
    write(&edited.join("notes.txt"), "unsaved\n");
    f.repo.find_worktree("locked").unwrap().lock(None).unwrap();

    for (name, path, expected) in [
        ("moved", &moved, "HEAD changed"),
        ("edited", &edited, "working tree changed"),
        ("locked", &locked, "locked"),
    ] {
        let outcome = remove::run(&request(&f, &model, name), &journal, || false, || {});
        match outcome {
            Outcome::Blocked { reason } => assert!(reason.contains(expected), "{name}: {reason}"),
            other => panic!("{name}: {other:?}"),
        }
        assert!(path.exists(), "{name} must be left in place");
    }
}

#[test]
fn missing_directory_only_prunes_the_registration() {
    let f = Fixture::new();
    f.branch("gone", f.tip("main"));
    let path = f.worktree("gone", "gone", None);
    fs::remove_dir_all(&path).unwrap();
    let (model, _engine, _events) = scan(f.config(), f.journal());

    let outcome = remove::run(&request(&f, &model, "gone"), &f.journal(), || false, || {});
    let Outcome::Removed { report } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(report.directory, Step::AlreadyAbsent);
    assert_eq!(report.registration, Step::Done);
}

#[test]
fn cancellation_before_destructive_work_keeps_the_worktree() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    let path = f.worktree("feature", "feature", None);
    let (model, _engine, _events) = scan(f.config(), f.journal());

    let outcome = remove::run(
        &request(&f, &model, "feature"),
        &f.journal(),
        || true,
        || panic!("destructive work must not start"),
    );
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(path.exists());
}

#[test]
fn engine_never_queues_a_candidate_twice_and_honours_cancellation() {
    let f = Fixture::new();
    let names = ["w1", "w2", "w3", "w4", "w5", "w6"];
    for name in names {
        f.branch(name, f.tip("main"));
        f.worktree(name, name, None);
    }
    let mut config = f.config();
    config.workers.delete = 1;
    let (model, engine, events) = scan(config, f.journal());

    let first: Vec<Request> = names.iter().map(|name| request(&f, &model, name)).collect();
    let accepted = engine.submit(first.clone());
    assert_eq!(accepted.len(), names.len());
    // Bulk and delete-now overlap: nothing is queued twice.
    assert!(engine.submit(first[..2].to_vec()).is_empty());

    let cancelled: Vec<_> = names[3..]
        .iter()
        .filter(|name| engine.cancel(&f.id(name)) == CancelResult::Cancelled)
        .map(|name| f.id(name))
        .collect();

    let mut finished = std::collections::HashMap::new();
    let expected = names.len() - cancelled.len();
    while finished.len() < expected {
        if let Event::Delete(id, DeleteState::Finished(outcome)) =
            events.recv_timeout(Duration::from_secs(20)).unwrap()
        {
            assert!(finished.insert(id, outcome).is_none(), "finished twice");
        }
    }
    for id in &cancelled {
        assert!(!finished.contains_key(id));
        assert!(row(&model, id).candidate.seed.path.exists());
    }
    for (id, outcome) in &finished {
        let path = &row(&model, id).candidate.seed.path;
        match outcome {
            Outcome::Removed { .. } => assert!(!path.exists()),
            Outcome::Cancelled => assert!(path.exists()),
            other => panic!("{id}: {other:?}"),
        }
    }
}

#[test]
fn journal_reports_interrupted_operations_once() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    let path = f.worktree("feature", "feature", None);
    let journal = f.journal();
    journal
        .append(&JournalEntry {
            op: "op-1".into(),
            at: 0,
            id: f.id("feature"),
            path: path.clone(),
            gitdir: f.common.join("worktrees/feature"),
            phase: Phase::Intent,
            detail: None,
        })
        .unwrap();

    let interrupted = journal.reconcile().unwrap();
    assert_eq!(interrupted.len(), 1);
    assert!(interrupted[0].directory_exists);
    assert!(interrupted[0].registration_exists);
    assert!(journal.reconcile().unwrap().is_empty());
    assert!(path.exists(), "reconciliation never resumes deletion");
}

#[test]
fn status_counts_changes_untracked_files_and_skips_ignored_ones() {
    let f = Fixture::new();
    f.branch("feature", f.tip("main"));
    let path = f.worktree("feature", "feature", None);
    assert!(git_yard::assess::dirty(&path).unwrap().is_clean());

    write(&path.join("README"), "edited\n");
    write(&path.join(".gitignore"), "build/\n");
    fs::create_dir_all(path.join("build")).unwrap();
    write(&path.join("build/out"), "artifact");
    fs::create_dir_all(path.join("notes/deep")).unwrap();
    write(&path.join("notes/deep/a"), "a");
    write(&path.join("notes/b"), "b");
    let staged = path.join("staged");
    write(&staged, "new\n");
    let worktree_repo = git2::Repository::open(&path).unwrap();
    let mut index = worktree_repo.index().unwrap();
    index.add_path(std::path::Path::new("staged")).unwrap();
    index.write().unwrap();

    let dirty = git_yard::assess::dirty(&path).unwrap();
    // README modified and `staged` added; `.gitignore` and the collapsed
    // `notes/` directory are untracked; `build/` is ignored.
    assert_eq!(dirty.changed, 2, "{dirty:?}");
    assert_eq!(dirty.untracked, 2, "{dirty:?}");
    assert_eq!(dirty.conflicted, 0);
}
