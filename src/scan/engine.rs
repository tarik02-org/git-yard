//! Background work: discovery and inventory, assessment, measurement and
//! removal on bounded pools, reported as events. Reads are cancellable;
//! removals finish the protocol step they are in.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::core::candidate::{Candidate, Kind, Seed};
use crate::core::config::Config;
use crate::core::facts::{DirState, GitFacts, StorageFacts};
use crate::core::ids::{RepoKey, WorktreeKey, now};
use crate::core::obs::Obs;
use crate::fs::measure::measure;
use crate::fs::walk::{FoundRepo, normalize_roots, walk};
use crate::git::assess;
use crate::git::inventory::inventory;
use crate::git::open::Registry;
use crate::removal::protocol::{self, Context};
use crate::removal::request::{Outcome, Request};
use crate::store::journal::Journal;
use crate::store::lock::RepoLocks;

use super::cancel::CancelToken;

#[derive(Debug)]
pub enum Event {
    Cached(Vec<Candidate>),
    Seed(Seed),
    Git {
        key: WorktreeKey,
        observation: Obs<GitFacts>,
        watermark: Option<i64>,
    },
    Storage {
        key: WorktreeKey,
        observation: Obs<StorageFacts>,
    },
    RepoFailed {
        common_dir: PathBuf,
        error: String,
    },
    DiscoveryDone {
        repos: usize,
        elapsed: Duration,
    },
    Removal {
        key: WorktreeKey,
        state: RemovalState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalState {
    Queued { batch: u64 },
    Validating,
    Removing,
    Finished(Outcome),
}

impl RemovalState {
    /// Queued or running.
    pub fn is_active(&self) -> bool {
        !matches!(self, RemovalState::Finished(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    Off,
    /// Every candidate, most promising first.
    All,
    /// Only candidates named by `set_wanted` or `prioritize`.
    OnDemand,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CancelResult {
    /// Not started; it will not run.
    Cancelled,
    /// Validating; no change will be made.
    Requested,
    /// Changes have started and finish.
    Running,
    NotQueued,
}

/// What the engine needs from the outside world.
#[derive(Clone)]
pub struct Services {
    pub registry: Arc<Registry>,
    pub journal: Arc<Journal>,
    pub locks: Arc<RepoLocks>,
}

#[derive(Default)]
struct Book {
    /// Key → generation of the request waiting for it.
    queued: HashMap<WorktreeKey, u64>,
    started: HashSet<WorktreeKey>,
    removing: HashSet<WorktreeKey>,
    cancelled: HashSet<WorktreeKey>,
}

struct Shared {
    events: Sender<Event>,
    services: Services,
    config: Arc<Config>,
    measure_mode: Measure,
    /// Cancels every read; removals do not listen to it.
    reads: CancelToken,
    stopped: AtomicBool,
    measure: MeasureQueue,
    gate: Gate,
    book: Mutex<Book>,
    walk_pool: Arc<ThreadPool>,
    discovering: AtomicBool,
    assessing: AtomicUsize,
}

impl Shared {
    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopping(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    assess_tx: Sender<Seed>,
    delete_tx: Option<Sender<Request>>,
    delete_workers: Vec<JoinHandle<()>>,
}

impl Engine {
    pub fn start(
        config: Arc<Config>,
        services: Services,
        cached: Vec<Candidate>,
        measure_mode: Measure,
    ) -> (Engine, Receiver<Event>) {
        let (events, receiver) = unbounded();
        let shared = Arc::new(Shared {
            events,
            services,
            config: config.clone(),
            measure_mode,
            reads: CancelToken::new(),
            stopped: AtomicBool::new(false),
            measure: MeasureQueue::new(measure_mode == Measure::All),
            gate: Gate::default(),
            book: Mutex::default(),
            walk_pool: Arc::new(
                ThreadPoolBuilder::new()
                    .num_threads(config.workers.walk)
                    .thread_name(|index| format!("walk-{index}"))
                    .build()
                    .expect("spawning directory walker threads"),
            ),
            discovering: AtomicBool::new(true),
            assessing: AtomicUsize::new(0),
        });
        if !cached.is_empty() {
            shared.send(Event::Cached(cached));
        }

        let (assess_tx, assess_rx) = bounded::<Seed>(4096);
        for _ in 0..config.workers.scan {
            let (shared, assess_rx) = (shared.clone(), assess_rx.clone());
            thread::spawn(move || assess_worker(&shared, &assess_rx));
        }
        if measure_mode != Measure::Off {
            for _ in 0..config.workers.measure {
                let shared = shared.clone();
                thread::spawn(move || measure_worker(&shared));
            }
        }
        let (delete_tx, delete_rx) = unbounded::<Request>();
        let delete_workers = (0..config.workers.delete)
            .map(|_| {
                let (shared, delete_rx) = (shared.clone(), delete_rx.clone());
                thread::spawn(move || {
                    for request in delete_rx {
                        remove_one(&shared, request);
                        shared.gate.update(|gate| gate.deleting -= 1);
                    }
                })
            })
            .collect();
        {
            let (shared, assess_tx) = (shared.clone(), assess_tx.clone());
            thread::spawn(move || discovery(&shared, &assess_tx));
        }
        (
            Engine {
                shared,
                assess_tx,
                delete_tx: Some(delete_tx),
                delete_workers,
            },
            receiver,
        )
    }

    /// Measures this candidate next. Only the latest call counts.
    pub fn prioritize(&self, key: &WorktreeKey) {
        self.shared.measure.focus(key);
    }

    /// The candidates to measure on demand, most important first.
    pub fn set_wanted(&self, keys: Vec<WorktreeKey>) {
        self.shared.measure.want(keys);
    }

    pub fn set_measure_paused(&self, paused: bool) {
        self.shared.gate.update(|gate| gate.paused = paused);
    }

    /// Re-reads one repository, for example after a removal was blocked.
    pub fn rescan(&self, repo: RepoKey) {
        let (shared, assess_tx) = (self.shared.clone(), self.assess_tx.clone());
        thread::spawn(move || {
            let roots = normalize_roots(&shared.config.roots);
            let found = FoundRepo {
                common_dir: repo.path().to_path_buf(),
                root: roots.first().cloned().unwrap_or_default(),
            };
            shared.services.registry.forget(repo.path());
            inventory_repo(&shared, &found, &roots, &assess_tx);
        });
    }

    /// Queues requests; returns the keys accepted. Rows already queued or
    /// running are skipped.
    pub fn submit(&self, requests: Vec<Request>) -> Vec<WorktreeKey> {
        let Some(delete_tx) = &self.delete_tx else {
            return Vec::new();
        };
        let mut accepted = Vec::new();
        let mut book = self.shared.book();
        for request in requests {
            if book.queued.contains_key(&request.key) || book.started.contains(&request.key) {
                continue;
            }
            book.queued.insert(request.key.clone(), request.generation);
            accepted.push(request.key.clone());
            if delete_tx.send(request).is_ok() {
                self.shared.gate.update(|gate| gate.deleting += 1);
            }
        }
        accepted
    }

    pub fn cancel(&self, key: &WorktreeKey) -> CancelResult {
        let mut book = self.shared.book();
        if book.removing.contains(key) {
            CancelResult::Running
        } else if book.started.contains(key) {
            book.cancelled.insert(key.clone());
            CancelResult::Requested
        } else if book.queued.remove(key).is_some() {
            CancelResult::Cancelled
        } else {
            CancelResult::NotQueued
        }
    }

    /// Cancels reads and queued removals; running removals finish.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.shared.reads.cancel();
        self.shared.measure.close();
        self.shared.gate.update(|gate| gate.stopped = true);
    }

    /// Stops and waits until running removals reach a journaled end.
    pub fn shutdown(mut self) {
        self.stop();
        self.delete_tx = None;
        for worker in self.delete_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

fn discovery(shared: &Shared, assess_tx: &Sender<Seed>) {
    let started = Instant::now();
    let roots = normalize_roots(&shared.config.roots);
    let mut repos = 0;
    let mut directories: HashMap<(u64, u64), WorktreeKey> = HashMap::new();
    walk(
        &roots,
        shared.config.max_depth,
        &shared.config.skip_dirs,
        || !shared.stopping(),
        |found| {
            repos += 1;
            for mut seed in inventory_seeds(shared, &found, &roots) {
                // A checkout registered in two repositories belongs to neither.
                if let DirState::Present { dev, ino } = seed.dir
                    && seed.kind != Kind::BareRepository
                {
                    match directories.get(&(dev, ino)) {
                        Some(other) if other != &seed.key => {
                            seed.conflict = Some(format!("same directory as {other}"));
                        }
                        _ => {
                            directories.insert((dev, ino), seed.key.clone());
                        }
                    }
                }
                schedule(shared, seed, assess_tx);
            }
        },
    );
    shared.discovering.store(false, Ordering::Relaxed);
    shared.send(Event::DiscoveryDone {
        repos,
        elapsed: started.elapsed(),
    });
}

fn inventory_repo(shared: &Shared, found: &FoundRepo, roots: &[PathBuf], assess_tx: &Sender<Seed>) {
    for seed in inventory_seeds(shared, found, roots) {
        schedule(shared, seed, assess_tx);
    }
}

fn inventory_seeds(shared: &Shared, found: &FoundRepo, roots: &[PathBuf]) -> Vec<Seed> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let repo = shared.services.registry.open(&found.common_dir)?;
        inventory(&repo, found, roots)
    }));
    let error = match result {
        Ok(Ok(inventory)) => return inventory.seeds,
        Ok(Err(error)) => format!("{error:#}"),
        Err(panic) => format!("panicked: {}", panic_message(panic.as_ref())),
    };
    shared.send(Event::RepoFailed {
        common_dir: found.common_dir.clone(),
        error,
    });
    Vec::new()
}

/// The seed reaches the model before any observation of it.
fn schedule(shared: &Shared, seed: Seed, assess_tx: &Sender<Seed>) {
    shared.send(Event::Seed(seed.clone()));
    if shared.measure_mode != Measure::Off {
        if seed.dir.is_present() && seed.kind != Kind::BareRepository {
            shared.measure.push(
                seed.key.clone(),
                seed.path.to_path_buf(),
                measure_priority(&seed),
            );
        } else {
            shared.send(Event::Storage {
                key: seed.key.clone(),
                observation: Obs::Failed {
                    error: "no working directory".into(),
                },
            });
        }
    }
    shared.assessing.fetch_add(1, Ordering::Relaxed);
    if assess_tx.send(seed).is_err() {
        shared.assessing.fetch_sub(1, Ordering::Relaxed);
    }
}

fn assess_worker(shared: &Shared, seeds: &Receiver<Seed>) {
    for seed in seeds {
        shared.gate.wait_for_scan();
        if shared.stopping() {
            shared.assessing.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let token = shared.reads.child();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let repo = shared
                .services
                .registry
                .open(seed.key.common_dir.as_path())?;
            assess::observe(&repo, &seed, token.flag())
        }));
        shared.assessing.fetch_sub(1, Ordering::Relaxed);
        let (observation, watermark) = match result {
            Ok(Ok(Some((value, watermark)))) => (
                Obs::Complete {
                    value,
                    observed_at: now(),
                },
                Some(watermark),
            ),
            // Cancelled: no observation at all.
            Ok(Ok(None)) => continue,
            Ok(Err(error)) => (
                Obs::Failed {
                    error: format!("{error:#}"),
                },
                None,
            ),
            Err(panic) => (
                Obs::Failed {
                    error: format!("panicked: {}", panic_message(panic.as_ref())),
                },
                None,
            ),
        };
        shared.send(Event::Git {
            key: seed.key,
            observation,
            watermark,
        });
    }
}

/// Measurement competes with assessment for the disk, and assessment
/// decides eligibility, so walks wait for it briefly.
const MEASURE_HOLDOFF: Duration = Duration::from_secs(1);

fn measure_worker(shared: &Arc<Shared>) {
    let started = Instant::now();
    while (shared.discovering.load(Ordering::Relaxed)
        || shared.assessing.load(Ordering::Relaxed) > 0)
        && started.elapsed() < MEASURE_HOLDOFF
        && !shared.stopping()
    {
        thread::sleep(Duration::from_millis(5));
    }
    loop {
        shared.gate.wait_for_measure();
        let Some((key, path)) = shared.measure.pop() else {
            break;
        };
        let token = shared.reads.child();
        let (keep, hold) = (token.clone(), shared.clone());
        let result = catch_unwind(AssertUnwindSafe(|| {
            measure(
                &path,
                &shared.walk_pool,
                |partial| {
                    shared.send(Event::Storage {
                        key: key.clone(),
                        observation: Obs::Partial { value: *partial },
                    })
                },
                move || !keep.is_cancelled(),
                move || hold.gate.wait_for_measure(),
            )
        }));
        let observation = match result {
            Ok(Ok(Some(value))) => Obs::Complete {
                value,
                observed_at: now(),
            },
            Ok(Ok(None)) => continue,
            Ok(Err(error)) => Obs::Failed {
                error: error.to_string(),
            },
            Err(panic) => Obs::Failed {
                error: format!("panicked: {}", panic_message(panic.as_ref())),
            },
        };
        shared.send(Event::Storage { key, observation });
    }
}

fn remove_one(shared: &Shared, request: Request) {
    let key = request.key.clone();
    {
        let mut book = shared.book();
        if book.queued.get(&key) != Some(&request.generation) {
            // Cancelled or superseded; the UI already shows that.
            return;
        }
        book.started.insert(key.clone());
    }
    let outcome = if shared.stopping() {
        Outcome::Cancelled
    } else {
        shared.send(Event::Removal {
            key: key.clone(),
            state: RemovalState::Validating,
        });
        let cancelled = || shared.stopping() || shared.book().cancelled.contains(&key);
        let on_removing = || {
            shared.book().removing.insert(key.clone());
            shared.send(Event::Removal {
                key: key.clone(),
                state: RemovalState::Removing,
            });
        };
        let services = &shared.services;
        let context = Context {
            registry: &services.registry,
            journal: &services.journal,
            locks: &services.locks,
            cancelled: &cancelled,
            on_removing: &on_removing,
            probe: &protocol::no_probe,
        };
        match catch_unwind(AssertUnwindSafe(|| protocol::remove(&context, &request))) {
            Ok(Some(outcome)) => outcome,
            Ok(None) => unreachable!("no crash probe is installed"),
            Err(panic) => Outcome::Failed {
                reason: format!(
                    "panicked: {}; run `git-yard journal` to recover",
                    panic_message(panic.as_ref())
                ),
                quarantine: None,
            },
        }
    };
    {
        let mut book = shared.book();
        book.queued.remove(&key);
        book.started.remove(&key);
        book.removing.remove(&key);
        book.cancelled.remove(&key);
    }
    shared.send(Event::Removal {
        key,
        state: RemovalState::Finished(outcome),
    });
}

/// Linked worktrees first; within each kind, older Git activity first.
fn measure_priority(seed: &Seed) -> i64 {
    let age = seed
        .git_activity
        .map_or(0, |activity| (now() - activity).max(0));
    match seed.kind {
        Kind::LinkedWorktree => (1 << 40) + age,
        Kind::MainWorktree | Kind::BareRepository => age,
    }
}

/// Holds scans back: removal gets the disk to itself, and the user can
/// pause measurement.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    paused: bool,
    deleting: usize,
    stopped: bool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut GateState)) {
        change(&mut self.lock());
        self.changed.notify_all();
    }

    fn wait_while(&self, closed: impl Fn(&GateState) -> bool) {
        let mut state = self.lock();
        while !state.stopped && closed(&state) {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn wait_for_scan(&self) {
        self.wait_while(|state| state.deleting > 0);
    }

    fn wait_for_measure(&self) {
        self.wait_while(|state| state.paused || state.deleting > 0);
    }
}

struct MeasureQueue {
    state: Mutex<MeasureState>,
    ready: Condvar,
}

struct MeasureState {
    jobs: HashMap<WorktreeKey, (PathBuf, i64)>,
    /// `None` measures every job by priority.
    wanted: Option<Vec<WorktreeKey>>,
    focus: Option<WorktreeKey>,
    closed: bool,
}

impl MeasureQueue {
    fn new(everything: bool) -> Self {
        MeasureQueue {
            state: Mutex::new(MeasureState {
                jobs: HashMap::new(),
                wanted: (!everything).then(Vec::new),
                focus: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MeasureState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, key: WorktreeKey, path: PathBuf, priority: i64) {
        self.lock().jobs.insert(key, (path, priority));
        self.ready.notify_one();
    }

    fn focus(&self, key: &WorktreeKey) {
        self.lock().focus = Some(key.clone());
        self.ready.notify_one();
    }

    fn want(&self, keys: Vec<WorktreeKey>) {
        self.lock().wanted = Some(keys);
        self.ready.notify_all();
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.jobs.clear();
        self.ready.notify_all();
    }

    fn pop(&self) -> Option<(WorktreeKey, PathBuf)> {
        let mut state = self.lock();
        loop {
            if let Some(key) = state.next() {
                let (path, _) = state.jobs.remove(&key).expect("next job is waiting");
                return Some((key, path));
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl MeasureState {
    fn next(&self) -> Option<WorktreeKey> {
        let waiting = |key: &&WorktreeKey| self.jobs.contains_key(*key);
        if let Some(key) = self.focus.as_ref().filter(waiting) {
            return Some(key.clone());
        }
        match &self.wanted {
            Some(wanted) => wanted.iter().find(waiting).cloned(),
            None => self
                .jobs
                .iter()
                .max_by_key(|(key, (_, priority))| (*priority, Reverse(*key)))
                .map(|(key, _)| key.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> WorktreeKey {
        WorktreeKey::linked(&RepoKey::new("/repo/.git"), name)
    }

    fn drain(queue: &MeasureQueue) -> Vec<WorktreeKey> {
        let mut state = queue.lock();
        let mut popped = Vec::new();
        while let Some(key) = state.next() {
            state.jobs.remove(&key);
            popped.push(key);
        }
        popped
    }

    #[test]
    fn on_demand_measures_focus_then_wanted_order_only() {
        let queue = MeasureQueue::new(false);
        for (name, priority) in [("a", 3), ("b", 2), ("c", 1), ("d", 0)] {
            queue.push(key(name), PathBuf::from(name), priority);
        }
        assert_eq!(drain(&queue), Vec::<WorktreeKey>::new());
        queue.want(vec![key("c"), key("a")]);
        queue.focus(&key("d"));
        assert_eq!(drain(&queue), vec![key("d"), key("c"), key("a")]);
    }

    #[test]
    fn measuring_everything_follows_priority() {
        let queue = MeasureQueue::new(true);
        for (name, priority) in [("a", 1), ("b", 3), ("c", 2)] {
            queue.push(key(name), PathBuf::from(name), priority);
        }
        assert_eq!(drain(&queue), vec![key("b"), key("c"), key("a")]);
    }
}
