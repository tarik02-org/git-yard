//! Background work: discovery, assessment, measurement and deletion on
//! separate bounded worker pools, reporting incremental results as events.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::assess::{self, RepoCache};
use crate::config::Config;
use crate::discovery::{self, ConflictTracker, FoundRepo};
use crate::measure;
use crate::model::{Candidate, CandidateId, DirState, GitFacts, Kind, Obs, Seed, Usage, now};
use crate::remove::{self, Outcome, Request};
use crate::store::Journal;

#[derive(Debug)]
pub enum Event {
    Cached(Vec<Candidate>),
    Seed(Seed),
    Conflict { id: CandidateId, reason: String },
    Git(CandidateId, Obs<GitFacts>),
    Usage(CandidateId, Obs<Usage>),
    RepoFailed { common_dir: PathBuf, error: String },
    DiscoveryDone { repos: usize, elapsed: Duration },
    Delete(CandidateId, DeleteState),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteState {
    Queued { batch: u64 },
    Validating,
    Removing,
    Finished(Outcome),
}

impl DeleteState {
    /// Queued or running; the candidate cannot be submitted again.
    pub fn is_active(&self) -> bool {
        !matches!(self, DeleteState::Finished(_))
    }

    /// Whether the candidate may be selected for a new submission.
    pub fn allows_resubmission(&self) -> bool {
        matches!(
            self,
            DeleteState::Finished(
                Outcome::Cancelled | Outcome::Blocked { .. } | Outcome::Failed { .. }
            )
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub measure: Measure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    Off,
    /// Every candidate, most promising first.
    All,
    /// Only candidates named by `Engine::set_wanted` or `Engine::prioritize`.
    OnDemand,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CancelResult {
    /// Not started; it will not run.
    Cancelled,
    /// Validation is in progress; destructive work will not start.
    Requested,
    /// Destructive work has started and cannot be cancelled.
    Running,
    NotQueued,
}

#[derive(Default)]
struct DeleteBook {
    queued: HashSet<CandidateId>,
    started: HashSet<CandidateId>,
    removing: HashSet<CandidateId>,
    cancelled: HashSet<CandidateId>,
}

struct Shared {
    events: Sender<Event>,
    shutdown: AtomicBool,
    measure: MeasureQueue,
    gate: Arc<Gate>,
    book: Mutex<DeleteBook>,
    repo_locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    journal: Arc<Journal>,
    config: Arc<Config>,
    options: Options,
    /// Directory readers shared by all measurements, so concurrent walks
    /// stay within one bounded set of threads.
    walk_pool: Arc<ThreadPool>,
    discovering: AtomicBool,
    assessing: AtomicUsize,
}

impl Shared {
    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn stopping(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn book(&self) -> MutexGuard<'_, DeleteBook> {
        self.book.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn repo_lock(&self, repo: &Path) -> Arc<Mutex<()>> {
        let mut locks = self
            .repo_locks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        locks.entry(repo.to_path_buf()).or_default().clone()
    }

    /// Queues assessment and measurement for a freshly scanned seed.
    fn schedule(&self, seed: &Seed, assess_tx: &Sender<Seed>) {
        self.assessing.fetch_add(1, Ordering::Relaxed);
        let _ = assess_tx.send(seed.clone());
        if self.options.measure == Measure::Off {
            return;
        }
        match seed.dir {
            DirState::Present { .. } if seed.kind != Kind::BareRepository => {
                self.measure
                    .push(seed.id.clone(), seed.path.clone(), measure_priority(seed));
            }
            _ => self.send(Event::Usage(
                seed.id.clone(),
                Obs::Failed {
                    error: "no working directory".into(),
                },
            )),
        }
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
        journal: Arc<Journal>,
        cached: Vec<Candidate>,
        options: Options,
    ) -> (Engine, Receiver<Event>) {
        let (events, receiver) = unbounded();
        let shared = Arc::new(Shared {
            events,
            shutdown: AtomicBool::new(false),
            measure: MeasureQueue::new(options.measure == Measure::All),
            gate: Arc::default(),
            book: Mutex::default(),
            repo_locks: Mutex::default(),
            journal,
            config: config.clone(),
            options,
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

        let (assess_tx, assess_rx) = bounded::<Seed>(1024);
        for _ in 0..config.workers.scan {
            let shared = shared.clone();
            let assess_rx = assess_rx.clone();
            thread::spawn(move || assess_worker(&shared, &assess_rx));
        }
        for _ in 0..config.workers.measure {
            let shared = shared.clone();
            thread::spawn(move || measure_worker(&shared));
        }
        let (delete_tx, delete_rx) = unbounded::<Request>();
        let delete_workers = (0..config.workers.delete)
            .map(|_| {
                let shared = shared.clone();
                let delete_rx = delete_rx.clone();
                thread::spawn(move || delete_worker(&shared, &delete_rx))
            })
            .collect();

        {
            let shared = shared.clone();
            let assess_tx = assess_tx.clone();
            thread::spawn(move || discovery_thread(&shared, &assess_tx));
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

    /// Measures this candidate next, even outside the wanted set. Only the
    /// latest call counts, so scrolling through a list does not queue it all.
    pub fn prioritize(&self, id: &CandidateId) {
        self.shared.measure.focus(id);
    }

    /// The candidates worth measuring in on-demand mode, most important
    /// first. Replaces the previous set; running walks are not interrupted.
    pub fn set_wanted(&self, ids: Vec<CandidateId>) {
        self.shared.measure.want(ids);
    }

    /// Pauses size measurement, including walks in progress, until resumed.
    pub fn set_measure_paused(&self, paused: bool) {
        self.shared.gate.update(|gate| gate.paused = paused);
    }

    /// Re-reads a repository's registrations and reassesses them, for
    /// example after a removal was blocked by changed evidence.
    pub fn rescan_repo(&self, common_dir: PathBuf) {
        let shared = self.shared.clone();
        let assess_tx = self.assess_tx.clone();
        thread::spawn(move || {
            let roots = discovery::normalize_roots(&shared.config.roots);
            let found = FoundRepo {
                root: common_dir.clone(),
                common_dir,
            };
            scan_one(&shared, &found, &roots, &assess_tx, None);
        });
    }

    /// Queues removal requests as one batch. Returns the identities that were
    /// accepted; candidates already queued or running are skipped.
    pub fn submit(&self, requests: Vec<Request>) -> Vec<CandidateId> {
        let Some(delete_tx) = &self.delete_tx else {
            return Vec::new();
        };
        let mut accepted = Vec::new();
        let mut book = self.shared.book();
        for request in requests {
            if !book.queued.insert(request.id.clone()) {
                continue;
            }
            accepted.push(request.id.clone());
            let _ = delete_tx.send(request);
        }
        self.shared
            .gate
            .update(|gate| gate.deleting += accepted.len());
        accepted
    }

    pub fn cancel(&self, id: &CandidateId) -> CancelResult {
        let mut book = self.shared.book();
        if book.removing.contains(id) {
            CancelResult::Running
        } else if book.started.contains(id) {
            book.cancelled.insert(id.clone());
            CancelResult::Requested
        } else if book.queued.remove(id) {
            CancelResult::Cancelled
        } else {
            CancelResult::NotQueued
        }
    }

    /// Stops scanning and new destructive work. Running removals continue.
    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.measure.close();
        self.shared.gate.update(|gate| gate.stopped = true);
    }

    /// Stops and waits for running removals to finish.
    pub fn shutdown(mut self) {
        self.stop();
        self.delete_tx = None;
        for worker in self.delete_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn discovery_thread(shared: &Shared, assess_tx: &Sender<Seed>) {
    let started = Instant::now();
    let roots = discovery::normalize_roots(&shared.config.roots);
    let mut conflicts = ConflictTracker::default();
    let mut repos = 0;
    let config = &shared.config;
    discovery::walk(&roots, config.max_depth, &config.skip_dirs, |found| {
        if shared.stopping() {
            return;
        }
        repos += 1;
        scan_one(shared, &found, &roots, assess_tx, Some(&mut conflicts));
    });
    shared.discovering.store(false, Ordering::Relaxed);
    shared.send(Event::DiscoveryDone {
        repos,
        elapsed: started.elapsed(),
    });
}

fn scan_one(
    shared: &Shared,
    found: &FoundRepo,
    roots: &[PathBuf],
    assess_tx: &Sender<Seed>,
    mut conflicts: Option<&mut ConflictTracker>,
) {
    match discovery::scan_repo(found, roots) {
        Ok(scan) => {
            for mut seed in scan.seeds {
                if let Some(conflicts) = conflicts.as_deref_mut()
                    && let Some(other) = conflicts.check(&seed)
                {
                    seed.conflict = Some(format!("same directory as {other}"));
                    shared.send(Event::Conflict {
                        reason: format!("same directory as {}", seed.id),
                        id: other,
                    });
                }
                // The seed must reach the model before any observation of it.
                shared.send(Event::Seed(seed.clone()));
                shared.schedule(&seed, assess_tx);
            }
        }
        Err(error) => shared.send(Event::RepoFailed {
            common_dir: found.common_dir.clone(),
            error: error.message().to_owned(),
        }),
    }
}

fn assess_worker(shared: &Shared, seeds: &Receiver<Seed>) {
    let mut cache = RepoCache::default();
    for seed in seeds {
        shared.gate.wait_for_scan();
        if shared.stopping() {
            return;
        }
        let observation = match assess::assess(&seed, &mut cache) {
            Ok(value) => Obs::Complete {
                value,
                observed_at: now(),
            },
            Err(error) => Obs::Failed {
                error: format!("{error:#}"),
            },
        };
        shared.assessing.fetch_sub(1, Ordering::Relaxed);
        shared.send(Event::Git(seed.id, observation));
    }
}

/// Measurement competes with assessment for filesystem throughput, and
/// assessment decides eligibility, so walks wait for it to drain first. The
/// wait is capped so huge workspaces still get sizes early.
const MEASURE_HOLDOFF: Duration = Duration::from_secs(1);

fn measure_worker(shared: &Shared) {
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
        let Some((id, path)) = shared.measure.pop() else {
            break;
        };
        let gate = shared.gate.clone();
        let result = measure::measure(
            &path,
            &shared.walk_pool,
            |partial| shared.send(Event::Usage(id.clone(), Obs::Partial { value: *partial })),
            || !shared.stopping(),
            move || gate.wait_for_measure(),
        );
        let observation = match result {
            Ok(value) => Obs::Complete {
                value,
                observed_at: now(),
            },
            Err(error) => Obs::Failed {
                error: error.to_string(),
            },
        };
        shared.send(Event::Usage(id, observation));
    }
}

fn delete_worker(shared: &Shared, jobs: &Receiver<Request>) {
    for request in jobs {
        delete_one(shared, request);
        shared.gate.update(|gate| gate.deleting -= 1);
    }
}

fn delete_one(shared: &Shared, request: Request) {
    let id = request.id.clone();
    {
        let mut book = shared.book();
        if !book.queued.contains(&id) {
            // Cancelled before it started; the UI already shows that.
            return;
        }
        book.started.insert(id.clone());
    }
    let outcome = if shared.stopping() {
        Outcome::Cancelled
    } else {
        let lock = shared.repo_lock(&id.repo);
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        shared.send(Event::Delete(id.clone(), DeleteState::Validating));
        remove::run(
            &request,
            &shared.journal,
            || shared.stopping() || shared.book().cancelled.contains(&id),
            || {
                shared.book().removing.insert(id.clone());
                shared.send(Event::Delete(id.clone(), DeleteState::Removing));
            },
        )
    };
    {
        let mut book = shared.book();
        book.queued.remove(&id);
        book.started.remove(&id);
        book.removing.remove(&id);
        book.cancelled.remove(&id);
    }
    shared.send(Event::Delete(id, DeleteState::Finished(outcome)));
}

/// Linked worktrees before main worktrees; within each, older Git activity
/// first, since old worktrees are the likeliest cleanup candidates.
fn measure_priority(seed: &Seed) -> i64 {
    let age = seed
        .git_activity
        .map_or(0, |activity| (now() - activity).max(0));
    match seed.kind {
        Kind::LinkedWorktree => (1 << 40) + age,
        Kind::MainWorktree | Kind::BareRepository => age,
    }
}

/// Holds background scans back: deletion gets the disk to itself, and the
/// user can pause size measurement.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    paused: bool,
    /// Removal requests submitted and not yet finished.
    deleting: usize,
    stopped: bool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
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
                .unwrap_or_else(|error| error.into_inner());
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
    /// Waiting jobs with their default priority.
    jobs: HashMap<CandidateId, (PathBuf, i64)>,
    /// `None` measures every job by default priority.
    wanted: Option<Vec<CandidateId>>,
    focus: Option<CandidateId>,
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
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn push(&self, id: CandidateId, path: PathBuf, priority: i64) {
        self.lock().jobs.insert(id, (path, priority));
        self.ready.notify_one();
    }

    fn focus(&self, id: &CandidateId) {
        self.lock().focus = Some(id.clone());
        self.ready.notify_one();
    }

    fn want(&self, ids: Vec<CandidateId>) {
        self.lock().wanted = Some(ids);
        self.ready.notify_all();
    }

    /// Drops waiting jobs and releases the workers. Until then workers wait
    /// for more jobs, since repository rescans can add work at any time.
    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.jobs.clear();
        self.ready.notify_all();
    }

    fn pop(&self) -> Option<(CandidateId, PathBuf)> {
        let mut state = self.lock();
        loop {
            if let Some(id) = state.next() {
                let (path, _) = state.jobs.remove(&id).expect("next job is waiting");
                return Some((id, path));
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

impl MeasureState {
    fn next(&self) -> Option<CandidateId> {
        let waiting = |id: &&CandidateId| self.jobs.contains_key(*id);
        if let Some(id) = self.focus.as_ref().filter(waiting) {
            return Some(id.clone());
        }
        match &self.wanted {
            Some(wanted) => wanted.iter().find(waiting).cloned(),
            None => self
                .jobs
                .iter()
                .max_by_key(|(id, (_, priority))| (*priority, Reverse(*id)))
                .map(|(id, _)| id.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Registration;

    fn id(name: &str) -> CandidateId {
        CandidateId {
            repo: PathBuf::from("/repo/.git"),
            registration: Registration::Linked(name.into()),
        }
    }

    fn drain(queue: &MeasureQueue) -> Vec<CandidateId> {
        let mut state = queue.lock();
        let mut popped = Vec::new();
        while let Some(id) = state.next() {
            state.jobs.remove(&id);
            popped.push(id);
        }
        popped
    }

    #[test]
    fn on_demand_measures_focus_then_wanted_order_only() {
        let queue = MeasureQueue::new(false);
        for (name, priority) in [("a", 3), ("b", 2), ("c", 1), ("d", 0)] {
            queue.push(id(name), PathBuf::from(name), priority);
        }
        assert_eq!(drain(&queue), Vec::<CandidateId>::new());

        queue.want(vec![id("c"), id("a")]);
        queue.focus(&id("d"));
        assert_eq!(drain(&queue), vec![id("d"), id("c"), id("a")]);
    }

    #[test]
    fn measuring_everything_follows_priority() {
        let queue = MeasureQueue::new(true);
        for (name, priority) in [("a", 1), ("b", 3), ("c", 2)] {
            queue.push(id(name), PathBuf::from(name), priority);
        }
        assert_eq!(drain(&queue), vec![id("b"), id("c"), id("a")]);
    }
}
