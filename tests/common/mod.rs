#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use git2::{Oid, Repository, Signature, Time, WorktreeAddOptions};
use tempfile::TempDir;

use git_yard::config::Config;
use git_yard::engine::{Engine, Event, Measure, Options};
use git_yard::model::{CandidateId, Registration};
use git_yard::state::{Model, Row};
use git_yard::store::{Journal, Paths};

pub const OLD: i64 = 1_600_000_000;

pub struct Fixture {
    pub dir: TempDir,
    pub root: PathBuf,
    pub repo: Repository,
    pub common: PathBuf,
}

impl Fixture {
    /// `<root>/ns/project` with one commit on `main`.
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().join("work");
        let main = root.join("ns/project");
        fs::create_dir_all(&main).unwrap();
        let repo = Repository::init(&main).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let common = fs::canonicalize(main.join(".git")).unwrap();
        let fixture = Fixture {
            dir,
            root,
            repo,
            common,
        };
        fixture.commit("refs/heads/main", None, &[("README", "hello\n")], OLD);
        fixture
    }

    pub fn main_path(&self) -> PathBuf {
        self.root.join("ns/project")
    }

    pub fn tip(&self, branch: &str) -> Oid {
        self.repo
            .refname_to_id(&format!("refs/heads/{branch}"))
            .unwrap()
    }

    /// Commits `files` on top of `parent` (or the ref's current tip) and
    /// updates `refname` when given.
    pub fn commit(
        &self,
        refname: &str,
        parent: Option<Oid>,
        files: &[(&str, &str)],
        time: i64,
    ) -> Oid {
        let parent = parent.or_else(|| self.repo.refname_to_id(refname).ok());
        let parent_commit = parent.map(|oid| self.repo.find_commit(oid).unwrap());
        let parent_tree = parent_commit.as_ref().map(|commit| commit.tree().unwrap());
        let mut builder = self.repo.treebuilder(parent_tree.as_ref()).unwrap();
        for (name, content) in files {
            let blob = self.repo.blob(content.as_bytes()).unwrap();
            builder.insert(name, blob, 0o100644).unwrap();
        }
        let tree = self.repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::new("t", "t@example.com", &Time::new(time, 0)).unwrap();
        let parents: Vec<_> = parent_commit.iter().collect();
        let oid = self
            .repo
            .commit(None, &signature, &signature, "commit", &tree, &parents)
            .unwrap();
        if !refname.is_empty() {
            self.repo.reference(refname, oid, true, "commit").unwrap();
        }
        oid
    }

    pub fn branch(&self, name: &str, at: Oid) {
        let commit = self.repo.find_commit(at).unwrap();
        self.repo.branch(name, &commit, true).unwrap();
    }

    /// Adds a linked worktree for `branch` at `path` (default: next to the
    /// main worktree).
    pub fn worktree(&self, name: &str, branch: &str, path: Option<PathBuf>) -> PathBuf {
        let path = path.unwrap_or_else(|| self.root.join(format!("ns/project.{name}")));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let reference = self
            .repo
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap();
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&reference));
        self.repo.worktree(name, &path, Some(&options)).unwrap();
        fs::canonicalize(path).unwrap()
    }

    pub fn id(&self, name: &str) -> CandidateId {
        CandidateId {
            repo: self.common.clone(),
            registration: Registration::Linked(name.into()),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            roots: vec![self.root.clone()],
            stale_days: 0,
            ..Config::default()
        }
    }

    pub fn paths(&self) -> Paths {
        Paths::under(&self.dir.path().join("state"))
    }

    pub fn journal(&self) -> Arc<Journal> {
        Arc::new(Journal::open(&self.paths().journal_file).unwrap())
    }
}

/// Runs a full scan and returns the settled model plus the running engine.
pub fn scan(
    config: Config,
    journal: Arc<Journal>,
) -> (Model, Engine, crossbeam_channel::Receiver<Event>) {
    let config = Arc::new(config);
    let (engine, events) = Engine::start(
        config.clone(),
        journal,
        Vec::new(),
        Options {
            measure: Measure::All,
        },
    );
    let mut model = Model::new(config);
    while !(model.all_assessed() && model.all_measured()) {
        let event = events
            .recv_timeout(Duration::from_secs(20))
            .expect("scan did not settle");
        model.apply(event);
        model.settle();
    }
    (model, engine, events)
}

pub fn row<'a>(model: &'a Model, id: &CandidateId) -> &'a Row {
    model.rows.get(id).unwrap_or_else(|| {
        panic!(
            "no candidate {id}; have {:?}",
            model.rows.keys().collect::<Vec<_>>()
        )
    })
}

pub fn write(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
}
