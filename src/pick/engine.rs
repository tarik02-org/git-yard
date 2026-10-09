//! Background work for the picker: discovery and listing, fetches, and
//! forge queries on bounded worker pools, reported as events.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use super::forge::{self, Requests};
use super::{Listing, Project, list_project};
use crate::config::Config;
use crate::discovery::{self, FoundRepo};
use crate::model::now;

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub enum Event {
    Listed(Listing),
    ListFailed {
        common_dir: PathBuf,
        error: String,
    },
    DiscoveryDone {
        projects: usize,
    },
    /// A fetch finished; a fresh `Listed` for the project follows on success.
    Fetched {
        common_dir: PathBuf,
        error: Option<String>,
    },
    Requests {
        common_dir: PathBuf,
        result: Result<Requests, String>,
    },
}

struct Shared {
    events: Sender<Event>,
    config: Arc<Config>,
    roots: Vec<PathBuf>,
}

impl Shared {
    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn list(&self, found: &FoundRepo) {
        match list_project(found, &self.roots, &self.config) {
            Ok(listing) => self.send(Event::Listed(listing)),
            Err(error) => self.send(Event::ListFailed {
                common_dir: found.common_dir.clone(),
                error: error.message().to_owned(),
            }),
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    fetch_tx: Sender<Project>,
    forge_tx: Sender<Project>,
    /// Projects fetched or queried this session; each happens at most once.
    fetched: Mutex<HashSet<PathBuf>>,
    queried: Mutex<HashSet<PathBuf>>,
}

impl Engine {
    pub fn start(config: Arc<Config>) -> (Engine, Receiver<Event>) {
        let (events, receiver) = unbounded();
        let shared = Arc::new(Shared {
            events,
            roots: discovery::normalize_roots(&config.roots),
            config: config.clone(),
        });

        let (list_tx, list_rx) = bounded::<FoundRepo>(1024);
        for _ in 0..config.workers.scan {
            let shared = shared.clone();
            let list_rx = list_rx.clone();
            thread::spawn(move || {
                for found in list_rx {
                    shared.list(&found);
                }
            });
        }
        {
            let shared = shared.clone();
            thread::spawn(move || {
                let config = &shared.config;
                let mut projects = 0;
                discovery::walk(
                    &shared.roots,
                    config.max_depth,
                    &config.skip_dirs,
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
                for project in fetch_rx {
                    fetch_worker(&fetch_shared, &project);
                }
            });
            let (forge_shared, forge_rx) = (shared.clone(), forge_rx.clone());
            thread::spawn(move || {
                for project in forge_rx {
                    forge_worker(&forge_shared, &project);
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

    /// Fetches all remotes of the project unless that happened recently.
    /// Returns whether a fetch was queued.
    pub fn fetch(&self, project: &Project) -> bool {
        let mut fetched = self.fetched.lock().unwrap_or_else(|e| e.into_inner());
        if !fetched.insert(project.common_dir.clone()) {
            return false;
        }
        let window = Duration::from_secs(self.shared.config.pick.fetch_minutes.saturating_mul(60));
        if fetched_within(&project.common_dir, window) {
            return false;
        }
        self.fetch_tx.send(project.clone()).is_ok()
    }

    /// Refreshes the project's open requests. Returns whether a query was
    /// queued; projects on no known forge are skipped.
    pub fn query_requests(&self, project: &Project) -> bool {
        if forge::detect(project).is_none() {
            return false;
        }
        let mut queried = self.queried.lock().unwrap_or_else(|e| e.into_inner());
        if !queried.insert(project.common_dir.clone()) {
            return false;
        }
        self.forge_tx.send(project.clone()).is_ok()
    }
}

fn fetch_worker(shared: &Shared, project: &Project) {
    let mut command = Command::new("git");
    command
        .args(["fetch", "--all", "--prune", "--quiet"])
        .env("GIT_TERMINAL_PROMPT", "0");
    let error = run_quietly(command, &project.main_path, FETCH_TIMEOUT)
        .err()
        .map(|error| format!("{error:#}"));
    let fetched = error.is_none();
    shared.send(Event::Fetched {
        common_dir: project.common_dir.clone(),
        error,
    });
    if fetched {
        shared.list(&FoundRepo {
            common_dir: project.common_dir.clone(),
            root: project.root.clone(),
        });
    }
}

fn forge_worker(shared: &Shared, project: &Project) {
    let Some(kind) = forge::detect(project) else {
        return;
    };
    let result = forge::fetch(kind, &project.main_path)
        .map(|items| Requests {
            forge: kind,
            fetched_at: now(),
            items,
        })
        .map_err(|error| format!("{error:#}"));
    shared.send(Event::Requests {
        common_dir: project.common_dir.clone(),
        result,
    });
}

/// `FETCH_HEAD` is rewritten by every fetch.
fn fetched_within(common_dir: &Path, window: Duration) -> bool {
    fs::metadata(common_dir.join("FETCH_HEAD"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < window)
}

/// Runs `command` in `dir` in its own session without a controlling
/// terminal, so SSH or credential prompts fail instead of drawing over the
/// picker. Returns stdout; kills the process group after `timeout`.
pub(crate) fn run_quietly(mut command: Command, dir: &Path, timeout: Duration) -> Result<Vec<u8>> {
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (done_tx, done_rx) = bounded(1);
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let reader = thread::spawn(move || {
            let _ = stderr.read_to_end(&mut err);
            err
        });
        let _ = stdout.read_to_end(&mut out);
        let err = reader.join().unwrap_or_default();
        let _ = done_tx.send((out, err, child.wait()));
    });
    let Ok((out, err, status)) = done_rx.recv_timeout(timeout) else {
        // SAFETY: the child leads its own process group after setsid.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        bail!("timed out after {}s", timeout.as_secs());
    };
    let status = status?;
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let reason = err
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map_or_else(|| status.to_string(), str::to_owned);
        return Err(anyhow!(reason));
    }
    Ok(out)
}
