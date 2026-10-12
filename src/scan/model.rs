//! Rows assembled from engine events, with derived eligibility,
//! recommendation and rating. Shared by `list` and the cleanup UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::candidate::Candidate;
use crate::core::config::Config;
use crate::core::facts::DecisionRevision;
use crate::core::ids::{Timestamp, WorktreeKey, now};
use crate::core::obs::Obs;
use crate::core::policy::{self, Block, Rating, Recommendation};
use crate::core::selection::{Facts, Selection};
use crate::removal::request::Outcome;

use super::engine::{Event, RemovalState};

#[derive(Clone, Debug)]
pub struct Row {
    pub candidate: Candidate,
    /// Confirmed by this run rather than only loaded from cache.
    pub fresh: bool,
    pub rating: Option<Rating>,
    pub eligibility: Result<(), Block>,
    pub recommendation: Recommendation,
    pub removal: Option<RemovalState>,
}

impl Row {
    fn new(candidate: Candidate, fresh: bool) -> Self {
        Row {
            candidate,
            fresh,
            rating: None,
            eligibility: Err(Block::Pending),
            recommendation: Recommendation::default(),
            removal: None,
        }
    }

    fn derive(&mut self, config: &Config, now: Timestamp) {
        self.eligibility = policy::eligibility(&self.candidate);
        self.recommendation = policy::recommend(&self.candidate, config, now);
        self.rating = policy::rate(&self.candidate, config, now);
    }

    pub fn is_removed(&self) -> bool {
        matches!(&self.removal, Some(RemovalState::Finished(outcome)) if outcome.is_removed())
    }

    pub fn is_removal_active(&self) -> bool {
        self.removal.as_ref().is_some_and(RemovalState::is_active)
    }
}

#[derive(Debug, Default)]
pub struct Timings {
    pub first_candidate: Option<Duration>,
    pub discovery: Option<Duration>,
    pub assessed: Option<Duration>,
    pub measured: Option<Duration>,
}

pub struct Model {
    pub config: Arc<Config>,
    pub rows: HashMap<WorktreeKey, Row>,
    pub selection: Selection,
    pub now: Timestamp,
    pub discovery_done: bool,
    pub repos: usize,
    pub errors: Vec<String>,
    pub timings: Timings,
    started: Instant,
    /// Rows changed since the order was refreshed.
    dirty: bool,
}

impl Model {
    pub fn new(config: Arc<Config>) -> Self {
        Model {
            config,
            rows: HashMap::new(),
            selection: Selection::default(),
            now: now(),
            discovery_done: false,
            repos: 0,
            errors: Vec::new(),
            timings: Timings::default(),
            started: Instant::now(),
            dirty: false,
        }
    }

    /// Applies one event; returns a finished removal, if this was one.
    pub fn apply(&mut self, event: Event) -> Option<(WorktreeKey, Outcome)> {
        match event {
            Event::Cached(candidates) => {
                for candidate in candidates {
                    let key = candidate.key().clone();
                    if !self.rows.contains_key(&key) {
                        self.rows.insert(key.clone(), Row::new(candidate, false));
                        self.touch(&key);
                    }
                }
                self.mark_first_candidate();
            }
            Event::Seed(seed) => {
                let key = seed.key.clone();
                match self.rows.get_mut(&key) {
                    Some(row) => {
                        row.candidate.seed = seed;
                        let git = std::mem::replace(&mut row.candidate.git, Obs::Pending);
                        row.candidate.git = git.into_stale();
                        row.fresh = true;
                    }
                    None => {
                        self.rows
                            .insert(key.clone(), Row::new(Candidate::new(seed), true));
                    }
                }
                self.touch(&key);
                self.mark_first_candidate();
            }
            Event::Git {
                key,
                observation,
                watermark,
            } => {
                if let Some(row) = self.rows.get_mut(&key) {
                    row.candidate.git = observation;
                    row.candidate.watermark = watermark;
                    self.touch(&key);
                }
            }
            Event::Storage { key, observation } => {
                if let Some(row) = self.rows.get_mut(&key)
                    && !row.is_removed()
                {
                    // Partial walks are smaller than a cached total, so the
                    // cached value stays until the walk completes.
                    let previous = std::mem::replace(&mut row.candidate.storage, Obs::Pending);
                    row.candidate.storage = match (previous, observation) {
                        (stale @ Obs::Stale { .. }, Obs::Partial { .. }) => stale,
                        (_, next) => next,
                    };
                    self.touch(&key);
                }
            }
            Event::RepoFailed { common_dir, error } => {
                self.errors
                    .push(format!("{}: {error}", common_dir.display()));
            }
            Event::DiscoveryDone { repos, elapsed } => {
                self.discovery_done = true;
                self.repos = repos;
                self.timings.discovery = Some(elapsed);
                let vanished: Vec<WorktreeKey> = self
                    .rows
                    .iter()
                    .filter(|(_, row)| !row.fresh)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in vanished {
                    self.rows.remove(&key);
                    self.selection.remove(&key);
                }
            }
            Event::Removal { key, state } => {
                if let Some(row) = self.rows.get_mut(&key) {
                    let finished = match &state {
                        RemovalState::Finished(outcome) => Some((key.clone(), outcome.clone())),
                        _ => None,
                    };
                    row.removal = Some(state);
                    self.dirty = true;
                    return finished;
                }
            }
        }
        None
    }

    /// Re-derives time-dependent values.
    pub fn tick(&mut self) {
        self.now = now();
        let (config, now) = (self.config.clone(), self.now);
        for row in self.rows.values_mut() {
            row.derive(&config, now);
        }
        self.dirty = true;
    }

    /// Re-sorts once for all events since the last call and records timings.
    pub fn settle(&mut self) {
        if std::mem::take(&mut self.dirty) {
            self.selection.refresh(&self.rows);
        }
        if self.timings.assessed.is_none() && self.all_assessed() {
            self.timings.assessed = Some(self.started.elapsed());
        }
        if self.timings.measured.is_none() && self.all_measured() {
            self.timings.measured = Some(self.started.elapsed());
        }
    }

    pub fn set_removal(&mut self, key: &WorktreeKey, state: RemovalState) {
        if let Some(row) = self.rows.get_mut(key) {
            row.removal = Some(state);
            self.dirty = true;
        }
    }

    pub fn assessed(&self) -> usize {
        self.rows
            .values()
            .filter(|row| row.candidate.git.is_settled())
            .count()
    }

    pub fn measured(&self) -> usize {
        self.rows
            .values()
            .filter(|row| measure_settled(&row.candidate))
            .count()
    }

    pub fn all_assessed(&self) -> bool {
        self.discovery_done && self.assessed() == self.rows.len()
    }

    pub fn all_measured(&self) -> bool {
        self.discovery_done && self.measured() == self.rows.len()
    }

    /// Rows worth caching: confirmed this run, fully assessed, not removed.
    /// Unfinished measurements are left out.
    pub fn cacheable(&self) -> Vec<Candidate> {
        self.rows
            .values()
            .filter(|row| row.fresh && !row.is_removed() && row.candidate.git.complete().is_some())
            .map(|row| {
                let mut candidate = row.candidate.clone();
                if !matches!(candidate.storage, Obs::Complete { .. }) {
                    candidate.storage = Obs::Pending;
                }
                candidate
            })
            .collect()
    }

    fn touch(&mut self, key: &WorktreeKey) {
        let (config, now) = (self.config.clone(), self.now);
        if let Some(row) = self.rows.get_mut(key) {
            row.derive(&config, now);
        }
        self.selection.push(key.clone());
        self.dirty = true;
    }

    fn mark_first_candidate(&mut self) {
        if self.timings.first_candidate.is_none() {
            self.timings.first_candidate = Some(self.started.elapsed());
        }
    }
}

/// Measurement is over: done, failed, or impossible without a directory.
fn measure_settled(candidate: &Candidate) -> bool {
    candidate.storage.is_settled() || !candidate.seed.dir.is_present()
}

impl Facts for HashMap<WorktreeKey, Row> {
    fn rank(&self, key: &WorktreeKey) -> f64 {
        self.get(key)
            .and_then(|row| row.rating.as_ref())
            .map_or(f64::NEG_INFINITY, |rating| rating.score)
    }

    fn eligible(&self, key: &WorktreeKey) -> bool {
        self.get(key).is_some_and(|row| row.eligibility.is_ok())
    }

    fn recommended(&self, key: &WorktreeKey) -> bool {
        self.get(key)
            .is_some_and(|row| row.recommendation.recommended)
    }

    fn available(&self, key: &WorktreeKey) -> bool {
        self.get(key).is_some_and(|row| match &row.removal {
            None => true,
            Some(RemovalState::Finished(outcome)) => outcome.allows_retry(),
            Some(_) => false,
        })
    }

    fn revision(&self, key: &WorktreeKey) -> Option<DecisionRevision> {
        let row = self.get(key)?;
        row.candidate.git.complete()?;
        Some(row.candidate.revision())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::candidate::{Kind, Seed};
    use crate::core::facts::{DirState, GitFacts, Head, Integration, StorageFacts};
    use crate::core::ids::{RepoKey, YardPath};

    fn seed() -> Seed {
        Seed {
            key: WorktreeKey::linked(&RepoKey::new("/r/.git"), "w"),
            kind: Kind::LinkedWorktree,
            path: YardPath::new("/r.w"),
            repo_label: "r".into(),
            admin_dir: YardPath::new("/r/.git/worktrees/w"),
            lock: None,
            dir: DirState::Present { dev: 1, ino: 2 },
            git_activity: None,
            registration_error: None,
            conflict: None,
        }
    }

    #[test]
    fn unfinished_measurements_are_never_cached() {
        let mut model = Model::new(Arc::new(Config::default()));
        let key = seed().key;
        model.apply(Event::Seed(seed()));
        model.apply(Event::Git {
            key: key.clone(),
            observation: Obs::Complete {
                value: GitFacts {
                    head: Head::Unborn { name: "w".into() },
                    last_commit: None,
                    subject: None,
                    integration: Integration::Unborn,
                    status: None,
                    unreachable_commits: false,
                    in_progress: None,
                    private_modules: false,
                    bases: vec![],
                },
                observed_at: 0,
            },
            watermark: Some(0),
        });
        model.apply(Event::Storage {
            key: key.clone(),
            observation: Obs::Partial {
                value: StorageFacts::default(),
            },
        });
        assert_eq!(model.cacheable()[0].storage, Obs::Pending);
        model.apply(Event::Storage {
            key,
            observation: Obs::Complete {
                value: StorageFacts::default(),
                observed_at: 0,
            },
        });
        assert!(matches!(model.cacheable()[0].storage, Obs::Complete { .. }));
    }
}
