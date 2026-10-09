//! Candidate state assembled from engine events, with derived eligibility,
//! recommendation and rating. Shared by the TUI and the CLI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::engine::{DeleteState, Event};
use crate::model::{Candidate, CandidateId, Obs, Timestamp, now};
use crate::policy::{self, Block, Rating, Recommendation};
use crate::remove::Outcome;
use crate::selection::{Facts, Selection};

pub struct Row {
    pub candidate: Candidate,
    /// Confirmed by this run's discovery rather than only loaded from cache.
    pub fresh: bool,
    pub rating: Option<Rating>,
    pub eligibility: Result<(), Block>,
    pub recommendation: Recommendation,
    pub delete: Option<DeleteState>,
}

impl Row {
    fn new(candidate: Candidate, fresh: bool) -> Self {
        Row {
            candidate,
            fresh,
            rating: None,
            eligibility: Err(Block::Pending),
            recommendation: Recommendation::default(),
            delete: None,
        }
    }

    fn derive(&mut self, config: &Config, now: Timestamp) {
        self.eligibility = policy::eligibility(&self.candidate);
        self.recommendation = policy::recommend(&self.candidate, config, now);
        self.rating = policy::rate(&self.candidate, config, now);
    }

    pub fn is_removed(&self) -> bool {
        matches!(
            self.delete,
            Some(DeleteState::Finished(Outcome::Removed { .. }))
        )
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
    pub rows: HashMap<CandidateId, Row>,
    pub selection: Selection,
    pub now: Timestamp,
    pub discovery_done: bool,
    pub repos: usize,
    pub errors: Vec<String>,
    pub timings: Timings,
    started: Instant,
    /// Rows changed since the order was last refreshed.
    dirty: bool,
}

/// What an applied event changed, for callers that react to it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub finished: Option<(CandidateId, Outcome)>,
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

    pub fn apply(&mut self, event: Event) -> Applied {
        let mut applied = Applied::default();
        match event {
            Event::Cached(candidates) => {
                for candidate in candidates {
                    let id = candidate.id().clone();
                    if self.rows.contains_key(&id) {
                        continue;
                    }
                    self.rows.insert(id.clone(), Row::new(candidate, false));
                    self.touch(&id);
                }
                self.mark_first_candidate();
            }
            Event::Seed(seed) => {
                let id = seed.id.clone();
                match self.rows.get_mut(&id) {
                    Some(row) => {
                        row.candidate.seed = seed;
                        row.candidate.git =
                            demote(std::mem::replace(&mut row.candidate.git, Obs::Pending));
                        row.fresh = true;
                    }
                    None => {
                        self.rows
                            .insert(id.clone(), Row::new(Candidate::new(seed), true));
                    }
                }
                self.touch(&id);
                self.mark_first_candidate();
            }
            Event::Conflict { id, reason } => {
                if let Some(row) = self.rows.get_mut(&id) {
                    row.candidate.seed.conflict = Some(reason);
                    self.touch(&id);
                }
            }
            Event::Git(id, observation) => {
                if let Some(row) = self.rows.get_mut(&id) {
                    row.candidate.git = observation;
                    self.touch(&id);
                }
            }
            Event::Usage(id, observation) => {
                if let Some(row) = self.rows.get_mut(&id)
                    && !row.is_removed()
                {
                    row.candidate.usage = keep_stale_until_complete(
                        std::mem::replace(&mut row.candidate.usage, Obs::Pending),
                        observation,
                    );
                    self.touch(&id);
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
                let vanished: Vec<CandidateId> = self
                    .rows
                    .iter()
                    .filter(|(_, row)| !row.fresh)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in vanished {
                    self.rows.remove(&id);
                    self.selection.remove(&id);
                }
            }
            Event::Delete(id, state) => {
                if let Some(row) = self.rows.get_mut(&id) {
                    if let DeleteState::Finished(outcome) = &state {
                        applied.finished = Some((id.clone(), outcome.clone()));
                    }
                    row.delete = Some(state);
                    self.dirty = true;
                }
            }
        }
        applied
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

    /// Applies the effects of all events since the last call: re-sorts the
    /// list once and records stage timings. Applying events stays O(1) so a
    /// burst of thousands of events costs one sort, not thousands.
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

    pub fn set_delete_state(&mut self, id: &CandidateId, state: DeleteState) {
        if let Some(row) = self.rows.get_mut(id) {
            row.delete = Some(state);
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
            .filter(|row| row.candidate.usage.is_settled())
            .count()
    }

    pub fn all_assessed(&self) -> bool {
        self.discovery_done && self.assessed() == self.rows.len()
    }

    pub fn all_measured(&self) -> bool {
        self.discovery_done && self.measured() == self.rows.len()
    }

    fn touch(&mut self, id: &CandidateId) {
        let (config, now) = (self.config.clone(), self.now);
        if let Some(row) = self.rows.get_mut(id) {
            row.derive(&config, now);
        }
        if self.selection.position(id).is_none() {
            self.selection.push(id.clone());
        }
        self.dirty = true;
    }

    fn mark_first_candidate(&mut self) {
        if self.timings.first_candidate.is_none() {
            self.timings.first_candidate = Some(self.started.elapsed());
        }
    }
}

/// A rescanned candidate keeps its previous assessment visibly stale until
/// the fresh one arrives.
fn demote<T>(observation: Obs<T>) -> Obs<T> {
    match observation {
        Obs::Complete { value, observed_at } | Obs::Stale { value, observed_at } => {
            Obs::Stale { value, observed_at }
        }
        other => other,
    }
}

/// Partial measurements of a fresh walk are smaller than the cached total,
/// so the cached value stays visible until the walk completes.
fn keep_stale_until_complete<T>(previous: Obs<T>, next: Obs<T>) -> Obs<T> {
    match (previous, next) {
        (stale @ Obs::Stale { .. }, Obs::Partial { .. }) => stale,
        (_, next) => next,
    }
}

impl Facts for HashMap<CandidateId, Row> {
    fn rank(&self, id: &CandidateId) -> f64 {
        self.get(id)
            .and_then(|row| row.rating.as_ref())
            .map_or(f64::NEG_INFINITY, |rating| rating.score)
    }

    fn eligible(&self, id: &CandidateId) -> bool {
        self.get(id).is_some_and(|row| row.eligibility.is_ok())
    }

    fn recommended(&self, id: &CandidateId) -> bool {
        self.get(id).is_some_and(|row| row.recommendation.checked)
    }

    fn available(&self, id: &CandidateId) -> bool {
        self.get(id).is_some_and(|row| match &row.delete {
            None => true,
            Some(state) => state.allows_resubmission(),
        })
    }
}
