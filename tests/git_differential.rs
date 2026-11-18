//! In-process Git reads compared with the git command line on real
//! repositories, across the layouts git-yard has to survive.

mod common;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use common::{Env, worktree_list};
use git_yard::core::candidate::{Kind, Seed};
use git_yard::core::facts::{DirState, EntryKind, GitFacts, Head, Integration};
use git_yard::git::{assess, graph, head, inventory, refs, status};
use gix::ObjectId;

fn seed_at<'a>(seeds: &'a [Seed], path: &Path) -> &'a Seed {
    seeds
        .iter()
        .find(|seed| seed.path.as_path() == path)
        .unwrap_or_else(|| panic!("no seed at {}: {seeds:#?}", path.display()))
}

fn facts(repo: &gix::Repository, seed: &Seed) -> GitFacts {
    let bases = refs::base_refs(repo);
    assess::assess(repo, seed, &bases, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .unwrap()
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).unwrap()
}

#[test]
fn inventory_and_heads_match_worktree_list() {
    let env = Env::new();
    let main = env.repo("project");
    env.git(&main, &["branch", "feature"]);
    env.git(&main, &["branch", "locked"]);
    env.git(&main, &["branch", "gone"]);
    env.git(&main, &["branch", "unmounted"]);
    env.git(&main, &["branch", "linked"]);
    env.git(&main, &["branch", "relative"]);
    let feature = env.worktree(&main, "feature", &["feature"]);
    let detached = env.worktree(&main, "detached", &["--detach", "HEAD"]);
    let locked = env.worktree(
        &main,
        "locked",
        &["--lock", "--reason", "on a USB disk", "locked"],
    );
    let gone = env.worktree(&main, "gone", &["gone"]);
    fs::remove_dir_all(&gone).unwrap();
    let unmounted_parent = env.root.join("volume");
    fs::create_dir(&unmounted_parent).unwrap();
    let unmounted = unmounted_parent.join("wt");
    env.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--",
            unmounted.to_str().unwrap(),
            "unmounted",
        ],
    );
    fs::remove_dir_all(&unmounted_parent).unwrap();
    let linked = env.worktree(&main, "linked", &["linked"]);
    fs::rename(&linked, env.root.join("moved")).unwrap();
    std::os::unix::fs::symlink(env.root.join("moved"), &linked).unwrap();
    let relative = env.root.join("relative");
    env.git(
        &main,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "-q",
            "--",
            "../relative",
            "relative",
        ],
    );
    let orphan = env.worktree(&main, "orphan", &["--orphan", "-b", "orphan"]);

    let registry = env.registry();
    let (repo, inventory) = env.inventory(&registry, &main);
    let listed = worktree_list(&env, &main);
    assert_eq!(inventory.seeds.len(), listed.len(), "{listed:#?}");

    for record in &listed {
        let seed = seed_at(&inventory.seeds, &record.path);
        assert_eq!(
            seed.lock
                .as_ref()
                .map(|lock| lock.reason.clone().unwrap_or_default()),
            record.locked,
            "{}",
            record.path.display()
        );
        if seed.dir.is_present() {
            let worktree = inventory::worktree_repo(&repo, seed).unwrap();
            let head = head::read_head(&worktree).unwrap();
            assert_eq!(
                head.oid().map(str::to_owned),
                record.head.clone().filter(|h| !h.chars().all(|c| c == '0'))
            );
            match &head {
                Head::Branch { name, .. } | Head::Unborn { name } => {
                    assert_eq!(Some(name), record.branch.as_ref())
                }
                Head::Detached { .. } => assert!(record.detached),
            }
        }
    }

    assert_eq!(inventory.seeds[0].kind, Kind::MainWorktree);
    assert!(seed_at(&inventory.seeds, &feature).dir.is_present());
    assert!(seed_at(&inventory.seeds, &detached).dir.is_present());
    assert_eq!(
        seed_at(&inventory.seeds, &locked)
            .lock
            .as_ref()
            .unwrap()
            .reason
            .as_deref(),
        Some("on a USB disk")
    );
    assert_eq!(
        seed_at(&inventory.seeds, &gone).dir,
        DirState::Missing {
            parent_present: true
        }
    );
    assert_eq!(
        seed_at(&inventory.seeds, &unmounted).dir,
        DirState::Missing {
            parent_present: false
        }
    );
    assert_eq!(
        seed_at(&inventory.seeds, &linked).dir,
        DirState::NotADirectory {
            kind: EntryKind::Symlink
        }
    );
    let relative_seed = seed_at(&inventory.seeds, &relative);
    assert!(relative_seed.dir.is_present());
    assert_eq!(relative_seed.conflict, None);
    let orphan_seed = seed_at(&inventory.seeds, &orphan);
    let orphan_facts = facts(&repo, orphan_seed);
    assert_eq!(
        orphan_facts.head,
        Head::Unborn {
            name: "orphan".into()
        }
    );
    assert_eq!(orphan_facts.integration, Integration::Unborn);

    // Missing directories are still assessed from the registration.
    let gone_facts = facts(&repo, seed_at(&inventory.seeds, &gone));
    assert_eq!(gone_facts.head.branch(), Some("gone"));
    assert_eq!(gone_facts.status.unwrap().staged, 0);
}

#[test]
fn bare_repositories_list_the_bare_main_first() {
    let env = Env::new();
    let source = env.repo("source");
    let bare = env.root.join("bare.git");
    env.git(
        &env.root,
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let linked = env.root.join("bare.feature");
    env.git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            "--",
            linked.to_str().unwrap(),
        ],
    );

    let registry = env.registry();
    let (repo, inventory) = env.inventory(&registry, &bare);
    let listed = worktree_list(&env, &bare);
    assert!(listed[0].bare);
    assert_eq!(inventory.seeds[0].kind, Kind::BareRepository);
    assert_eq!(
        inventory.seeds[0].path.as_path(),
        fs::canonicalize(&bare).unwrap()
    );
    let seed = seed_at(&inventory.seeds, &linked);
    let facts = facts(&repo, seed);
    assert_eq!(facts.head.branch(), Some("feature"));
    assert_eq!(facts.head.oid(), listed[1].head.as_deref());
}

#[test]
fn graph_answers_match_merge_base_and_rev_list() {
    let env = Env::new();
    let main = env.repo("project");
    let base = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "-c", "merged"]);
    let merged = env.commit(&main, "a", "1\n");
    env.git(&main, &["switch", "-q", "main"]);
    env.git(&main, &["merge", "-q", "--ff-only", "merged"]);
    env.commit(&main, "b", "2\n");

    env.git(&main, &["switch", "-q", "-c", "squashed", &base]);
    env.commit(&main, "c", "3\n");
    env.commit(&main, "dir/d", "4\n");
    let squashed = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "main"]);
    env.git(&main, &["merge", "-q", "--squash", "squashed"]);
    env.git(&main, &["commit", "-q", "-m", "squash"]);

    env.git(&main, &["switch", "-q", "-c", "unmerged", &base]);
    env.commit(&main, "c", "different\n");
    env.commit(&main, "e", "5\n");
    let unmerged = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "main"]);
    let main_tip = env.git(&main, &["rev-parse", "HEAD"]);

    let registry = env.registry();
    let repo = registry.open(&env.common_dir(&main)).unwrap();
    for tip in [&merged, &squashed, &unmerged, &base] {
        let expected = env.git_ok(&main, &["merge-base", "--is-ancestor", tip, &main_tip]);
        assert_eq!(
            graph::is_merged(&repo, oid(tip), oid(&main_tip)).unwrap(),
            expected
        );
        let count: usize = env
            .git(
                &main,
                &["rev-list", "--count", &format!("{main_tip}..{tip}")],
            )
            .parse()
            .unwrap();
        assert_eq!(
            graph::ahead(&repo, oid(tip), oid(&main_tip)).unwrap(),
            count
        );
    }
    assert!(graph::changes_present(&repo, oid(&squashed), oid(&main_tip)).unwrap());
    assert!(!graph::changes_present(&repo, oid(&unmerged), oid(&main_tip)).unwrap());

    let bases = refs::base_refs(&repo);
    assert_eq!(bases.len(), 1);
    assert_eq!(
        graph::integration(&repo, Some("squashed"), oid(&squashed), &bases).unwrap(),
        Integration::ChangesPresent {
            into: "main".into()
        }
    );
    assert_eq!(
        graph::integration(&repo, Some("unmerged"), oid(&unmerged), &bases).unwrap(),
        Integration::NotIntegrated {
            base: "main".into(),
            ahead: 2
        }
    );
}

#[test]
fn squash_merges_are_found_after_the_base_edits_their_paths() {
    let env = Env::new();
    let main = env.repo("project");
    let base = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "-c", "squashed"]);
    env.commit(&main, "a", "1\n");
    env.commit(&main, "dir/b", "2\n");
    let squashed = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "main"]);
    env.git(&main, &["merge", "-q", "--squash", "squashed"]);
    env.git(&main, &["commit", "-q", "-m", "squash"]);
    env.commit(&main, "a", "reworked\n");
    env.commit(&main, "c", "3\n");

    env.git(&main, &["switch", "-q", "-c", "unmerged", &base]);
    env.commit(&main, "a", "never merged\n");
    let unmerged = env.git(&main, &["rev-parse", "HEAD"]);
    env.git(&main, &["switch", "-q", "main"]);
    let main_tip = env.git(&main, &["rev-parse", "HEAD"]);

    let registry = env.registry();
    let repo = registry.open(&env.common_dir(&main)).unwrap();
    assert!(graph::changes_present(&repo, oid(&squashed), oid(&main_tip)).unwrap());
    assert!(!graph::changes_present(&repo, oid(&unmerged), oid(&main_tip)).unwrap());
}

#[test]
fn base_refs_follow_the_announcing_remote() {
    let env = Env::new();
    let upstream = env.repo("upstream");
    env.git(&upstream, &["branch", "-m", "trunk"]);
    let clone = env.root.join("clone");
    env.git(
        &env.root,
        &[
            "clone",
            "-q",
            "-o",
            "up",
            upstream.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let registry = env.registry();
    let repo = registry.open(&env.common_dir(&clone)).unwrap();
    let names: Vec<String> = refs::base_refs(&repo)
        .into_iter()
        .map(|base| base.name)
        .collect();
    assert_eq!(names, ["trunk", "up/trunk"]);
}

#[test]
fn unreachable_commits_count_other_worktrees_and_their_private_refs() {
    let env = Env::new();
    let main = env.repo("project");
    let orphan_wt = env.worktree(&main, "orphan", &["--detach", "HEAD"]);
    let orphan = env.commit(&orphan_wt, "x", "1\n");
    let kept_wt = env.worktree(&main, "kept", &["--detach", "HEAD"]);
    env.git(&kept_wt, &["checkout", "-q", "--detach", &orphan]);
    // `holder` keeps a commit only in its private ref and moves on.
    let holder_wt = env.worktree(&main, "holder", &["--detach", "HEAD"]);
    let held = env.commit(&holder_wt, "y", "2\n");
    env.git(&holder_wt, &["update-ref", "refs/worktree/keep", &held]);
    env.git(&holder_wt, &["checkout", "-q", "--detach", "main"]);
    let inner_wt = env.worktree(&main, "inner", &["--detach", &held]);
    // `solo`'s commit is held only by its own HEAD and private ref.
    let solo_wt = env.worktree(&main, "solo", &["--detach", "HEAD"]);
    let solo = env.commit(&solo_wt, "z", "3\n");
    env.git(&solo_wt, &["update-ref", "refs/worktree/keep", &solo]);

    let registry = env.registry();
    let (repo, inventory) = env.inventory(&registry, &main);
    let unreachable =
        |path: &Path| facts(&repo, seed_at(&inventory.seeds, path)).unreachable_commits;
    // `orphan` is also checked out in `kept`, and vice versa.
    assert!(!unreachable(&orphan_wt));
    assert!(!unreachable(&kept_wt));
    assert!(!unreachable(&holder_wt));
    assert!(!unreachable(&inner_wt));
    assert!(unreachable(&solo_wt));
    assert_eq!(
        env.git(
            &main,
            &[
                "rev-list",
                "--count",
                &solo,
                "--not",
                "--branches",
                "--tags"
            ]
        ),
        "1"
    );
}

fn porcelain_counts(env: &Env, dir: &Path) -> (usize, usize, usize, usize, usize) {
    let output = env
        .command(
            dir,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignored=traditional",
            ],
        )
        .output()
        .unwrap();
    let (mut staged, mut changed, mut untracked, mut ignored, mut conflicted) = (0, 0, 0, 0, 0);
    let mut fields = output.stdout.split(|byte| *byte == 0).peekable();
    while let Some(field) = fields.next() {
        match field.first() {
            Some(b'1') => {
                staged += usize::from(field[2] != b'.');
                changed += usize::from(field[3] != b'.');
            }
            Some(b'2') => {
                staged += usize::from(field[2] != b'.');
                changed += usize::from(field[3] != b'.');
                fields.next();
            }
            Some(b'u') => conflicted += 1,
            Some(b'?') => untracked += 1,
            Some(b'!') => ignored += 1,
            _ => {}
        }
    }
    (staged, changed, untracked, ignored, conflicted)
}

#[test]
fn status_counts_match_porcelain_v2() {
    let env = Env::new();
    let main = env.repo("project");
    env.commit(&main, ".gitignore", "*.log\nbuild/\n");
    env.commit(&main, "tracked", "1\n");
    env.commit(&main, "removed", "1\n");
    let wt = env.worktree(&main, "dirty", &["-b", "dirty"]);
    fs::write(wt.join("tracked"), "2\n").unwrap();
    fs::remove_file(wt.join("removed")).unwrap();
    fs::write(wt.join("staged"), "new\n").unwrap();
    env.git(&wt, &["add", "staged"]);
    fs::create_dir_all(wt.join("notes/deep")).unwrap();
    fs::write(wt.join("notes/deep/a"), "x").unwrap();
    fs::write(wt.join("notes/b"), "y").unwrap();
    fs::write(wt.join("debug.log"), "log").unwrap();
    fs::create_dir_all(wt.join("build/out")).unwrap();
    fs::write(wt.join("build/out/bin"), "bin").unwrap();

    let registry = env.registry();
    let (repo, inventory) = env.inventory(&registry, &main);
    let worktree = inventory::worktree_repo(&repo, seed_at(&inventory.seeds, &wt)).unwrap();
    let evidence = status::worktree_status(&worktree, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .unwrap();
    let (staged, changed, untracked, ignored, conflicted) = porcelain_counts(&env, &wt);
    assert_eq!(evidence.staged, staged, "{evidence:?}");
    assert_eq!(evidence.changed, changed, "{evidence:?}");
    assert_eq!(evidence.untracked, untracked, "{evidence:?}");
    assert_eq!(evidence.ignored, ignored, "{evidence:?}");
    assert_eq!(evidence.conflicted, conflicted, "{evidence:?}");

    // Ignored files are not hashed; everything else is.
    fs::write(wt.join("debug.log"), "more").unwrap();
    let again = status::worktree_status(&worktree, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .unwrap();
    assert_eq!(again.hash, evidence.hash);
    fs::write(wt.join("notes/b"), "changed").unwrap();
    let changed_again = status::worktree_status(&worktree, Arc::new(AtomicBool::new(false)))
        .unwrap()
        .unwrap();
    assert_ne!(changed_again.hash, evidence.hash);

    // A directory that is gone keeps its staged changes in the index.
    fs::remove_dir_all(&wt).unwrap();
    let staged_only = status::staged_status(&worktree).unwrap();
    assert_eq!(staged_only.staged, staged);
}

#[test]
fn shallow_partial_and_split_index_repositories_assess() {
    let env = Env::new();
    let source = env.repo("source");
    env.commit(&source, "a", "1\n");
    env.commit(&source, "b", "2\n");
    env.git(&source, &["config", "uploadpack.allowFilter", "true"]);
    let url = format!("file://{}", source.display());

    for (name, args) in [
        ("shallow", vec!["--depth", "1"]),
        ("partial", vec!["--filter=blob:none"]),
    ] {
        let clone = env.root.join(name);
        let mut full = vec!["clone", "-q"];
        full.extend(args);
        full.extend([url.as_str(), clone.to_str().unwrap()]);
        env.git(&env.root, &full);
        let wt = env.worktree(&clone, "wt", &["-b", "wt"]);
        if name == "partial" {
            env.git(&wt, &["update-index", "--split-index"]);
            fs::write(wt.join("a"), "changed\n").unwrap();
        }
        let registry = env.registry();
        let (repo, inventory) = env.inventory(&registry, &clone);
        let facts = facts(&repo, seed_at(&inventory.seeds, &wt));
        assert_eq!(
            facts.head.oid().unwrap(),
            env.git(&wt, &["rev-parse", "HEAD"]),
            "{name}"
        );
        assert!(!facts.unreachable_commits, "{name}");
        let (_, changed, ..) = porcelain_counts(&env, &wt);
        assert_eq!(facts.status.unwrap().changed, changed, "{name}");
    }
}

#[test]
fn worktree_submodules_are_private_storage() {
    let env = Env::new();
    let library = env.repo("library");
    let main = env.repo("project");
    env.git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            library.to_str().unwrap(),
            "lib",
        ],
    );
    env.git(&main, &["commit", "-q", "-m", "submodule"]);
    let wt = env.worktree(&main, "wt", &["-b", "wt"]);
    env.git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "-q",
            "--init",
        ],
    );

    let registry = env.registry();
    let (repo, inventory) = env.inventory(&registry, &main);
    let facts = facts(&repo, seed_at(&inventory.seeds, &wt));
    assert!(facts.private_modules);
    assert!(facts.status.unwrap().is_clean(), "{:?}", facts.status);
}

#[test]
fn reftable_repositories_are_rejected() {
    let env = Env::new();
    let path = env.root.join("reftable");
    fs::create_dir(&path).unwrap();
    env.git(&path, &["init", "-q", "--ref-format=reftable"]);
    let error = env.registry().open(&env.common_dir(&path)).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported"), "{error:#}");
}
