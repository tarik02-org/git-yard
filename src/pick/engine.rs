//! Background work for the picker: discovery and listing, fetches, and
//! forge queries on bounded pools, reported as events.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::core::config::Config;
use crate::core::ids::{RepoKey, now};
use crate::fs::walk::{FoundRepo, normalize_roots, walk};
use crate::git::open::Registry;
use crate::git::process::Supervisor;
use crate::git::remote::project_forge;

use super::fetch::{self as fetching, fetched_within};
use super::forge::{self, Requests};
use super::listing::{Listing, Project, list_project};

#[derive(Debug)]
pub enum Event {
    Listed(Listing),
    ListFailed {
        repo: RepoKey,
        error: String,
    },
    DiscoveryDone {
        projects: usize,
    },
    /// A fetch finished; a fresh `Listed` follows on success.
    Fetched {
        repo: RepoKey,
        error: Option<String>,
    },
    Requests {
        repo: RepoKey,
        result: Result<Requests, String>,
    },
}

struct Shared {
    events: Sender<Event>,
    config: Arc<Config>,
    roots: Vec<PathBuf>,
    registry: Arc<Registry>,
    supervisor: Arc<Supervisor>,
}

impl Shared {
    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn list(&self, found: &FoundRepo) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let repo = self.registry.open(&found.common_dir)?;
            list_project(&repo, found, &self.roots, &self.config)
        }));
        let error = match result {
            Ok(Ok(listing)) => return self.send(Event::Listed(listing)),
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => "listing panicked".into(),
        };
        self.send(Event::ListFailed {
            repo: RepoKey::new(&found.common_dir),
            error,
        });
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    fetch_tx: Sender<Project>,
    forge_tx: Sender<Project>,
    /// Projects fetched or queried this session; each happens at most once.
    fetched: Mutex<HashSet<RepoKey>>,
    queried: Mutex<HashSet<RepoKey>>,
}

impl Engine {
    pub fn start(
        config: Arc<Config>,
        registry: Arc<Registry>,
        supervisor: Arc<Supervisor>,
    ) -> (Engine, Receiver<Event>) {
        let (events, receiver) = unbounded();
        let shared = Arc::new(Shared {
            events,
            roots: normalize_roots(&config.roots),
            config: config.clone(),
            registry,
            supervisor,
        });

        let (list_tx, list_rx) = bounded::<FoundRepo>(1024);
        for _ in 0..config.workers.scan {
            let (shared, list_rx) = (shared.clone(), list_rx.clone());
            thread::spawn(move || {
                for found in list_rx {
                    shared.list(&found);
                }
            });
        }
        {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut projects = 0;
                walk(
                    &shared.roots,
                    shared.config.max_depth,
                    &shared.config.skip_dirs,
                    || true,
                    |found| {
                        projects += 1;
                        let _ = list_tx.send(found);
                    },
                );
                drop(list_tx);
                shared.send(Event::DiscoveryDone { projects });
            });
        }

        let (fetch_tx, fetch_rx) = unbounded::<Project>();
        let (forge_tx, forge_rx) = unbounded::<Project>();
        for _ in 0..config.pick.jobs {
            let (fetch_shared, fetch_rx) = (shared.clone(), fetch_rx.clone());
            thread::spawn(move || {
                let shared = fetch_shared;
                for project in fetch_rx {
                    let error = fetching::fetch(&shared.supervisor, &project)
                        .err()
                        .map(|error| format!("{error:#}"));
                    let fetched = error.is_none();
                    shared.send(Event::Fetched {
                        repo: project.repo.clone(),
                        error,
                    });
                    if fetched {
                        shared.registry.forget(project.repo.path());
                        shared.list(&FoundRepo {
                            common_dir: project.repo.path().to_path_buf(),
                            root: shared.roots.first().cloned().unwrap_or_default(),
                        });
                    }
                }
            });
            let (forge_shared, forge_rx) = (shared.clone(), forge_rx.clone());
            thread::spawn(move || {
                let shared = forge_shared;
                for project in forge_rx {
                    let Some(forge) = project_forge(&project.remotes) else {
                        continue;
                    };
                    let result = forge::fetch(&shared.supervisor, &project, &forge)
                        .map(|items| Requests {
                            forge,
                            fetched_at: now(),
                            items,
                        })
                        .map_err(|error| format!("{error:#}"));
                    shared.send(Event::Requests {
                        repo: project.repo.clone(),
                        result,
                    });
                }
            });
        }

        (
            Engine {
                shared,
                fetch_tx,
                forge_tx,
                fetched: Mutex::default(),
                queried: Mutex::default(),
            },
            receiver,
        )
    }
}

/// What the picker asks of its engine.
pub trait Control {
    /// Fetches all remotes unless that happened recently. Returns whether a
    /// fetch was queued.
    fn fetch(&self, project: &Project) -> bool;
    /// Refreshes open requests. Returns whether a query was queued.
    fn query_requests(&self, project: &Project) -> bool;
}

impl Control for Engine {
    fn fetch(&self, project: &Project) -> bool {
        let mut fetched = self.fetched.lock().unwrap_or_else(PoisonError::into_inner);
        if !fetched.insert(project.repo.clone()) {
            return false;
        }
        let minutes = self.shared.config.pick.fetch_minutes;
        let window = Duration::from_secs(minutes.saturating_mul(60));
        if fetched_within(project.repo.path(), window) {
            return false;
        }
        self.fetch_tx.send(project.clone()).is_ok()
    }

    fn query_requests(&self, project: &Project) -> bool {
        if project_forge(&project.remotes).is_none() {
            return false;
        }
        let mut queried = self.queried.lock().unwrap_or_else(PoisonError::into_inner);
        if !queried.insert(project.repo.clone()) {
            return false;
        }
        self.forge_tx.send(project.clone()).is_ok()
    }
}
