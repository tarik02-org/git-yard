//! The ranked list, its cutoff, and per-candidate selection overrides.
//!
//! Rows above the cutoff that are eligible and recommended at the moment the
//! cutoff is placed start checked; others above it can be checked
//! individually. Rows below the cutoff show the state they would have, but
//! are disabled: never selected, not toggleable. Individual choices survive
//! cutoff movement. Background updates never change which rows the cutoff
//! selected; only moving the cutoff or reranking does.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::model::CandidateId;

pub trait Facts {
    /// Higher ranks first. Unrated candidates should return `f64::NEG_INFINITY`.
    fn rank(&self, id: &CandidateId) -> f64;
    fn eligible(&self, id: &CandidateId) -> bool;
    fn recommended(&self, id: &CandidateId) -> bool;
    /// False while queued, running, or removed.
    fn available(&self, id: &CandidateId) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleError {
    BelowCutoff,
    NotEligible,
    Unavailable,
}

#[derive(Debug, Default)]
pub struct Selection {
    order: Vec<CandidateId>,
    positions: HashMap<CandidateId, usize>,
    frozen: bool,
    cutoff: usize,
    auto: HashSet<CandidateId>,
    include: HashSet<CandidateId>,
    exclude: HashSet<CandidateId>,
}

impl Selection {
    pub fn order(&self) -> &[CandidateId] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn cutoff(&self) -> usize {
        self.cutoff
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    pub fn position(&self, id: &CandidateId) -> Option<usize> {
        self.positions.get(id).copied()
    }

    pub fn is_excluded(&self, id: &CandidateId) -> bool {
        self.exclude.contains(id)
    }

    /// Adds a new candidate. While frozen it goes to the end, below the
    /// cutoff, unchecked.
    pub fn insert(&mut self, id: CandidateId, facts: &impl Facts) {
        if self.positions.contains_key(&id) {
            return;
        }
        self.positions.insert(id.clone(), self.order.len());
        self.order.push(id);
        if !self.frozen {
            self.sort(facts);
        }
    }

    /// Appends a new candidate without sorting; `refresh` sorts later. While
    /// frozen it stays at the end, below the cutoff.
    pub fn push(&mut self, id: CandidateId) {
        if self.positions.contains_key(&id) {
            return;
        }
        self.positions.insert(id.clone(), self.order.len());
        self.order.push(id);
    }

    /// Re-sorts after rating changes unless the order is frozen.
    pub fn refresh(&mut self, facts: &impl Facts) {
        if !self.frozen {
            self.sort(facts);
        }
    }

    pub fn remove(&mut self, id: &CandidateId) {
        let Some(position) = self.positions.remove(id) else {
            return;
        };
        self.order.remove(position);
        if position < self.cutoff {
            self.cutoff -= 1;
        }
        self.auto.remove(id);
        self.include.remove(id);
        self.exclude.remove(id);
        self.reindex();
    }

    pub fn set_cutoff(&mut self, cutoff: usize, facts: &impl Facts) {
        self.frozen = true;
        self.cutoff = cutoff.min(self.order.len());
        self.recompute_auto(facts);
    }

    pub fn move_cutoff(&mut self, delta: isize, facts: &impl Facts) {
        let cutoff = self.cutoff.saturating_add_signed(delta);
        self.set_cutoff(cutoff, facts);
    }

    /// Flips an individual override and returns whether the row is now
    /// checked.
    pub fn toggle(&mut self, id: &CandidateId, facts: &impl Facts) -> Result<bool, ToggleError> {
        let Some(position) = self.position(id) else {
            return Err(ToggleError::Unavailable);
        };
        if position >= self.cutoff {
            return Err(ToggleError::BelowCutoff);
        }
        if !facts.available(id) {
            return Err(ToggleError::Unavailable);
        }
        if !facts.eligible(id) {
            return Err(ToggleError::NotEligible);
        }
        self.frozen = true;
        if self.is_checked(id, facts) {
            if !self.include.remove(id) || self.auto.contains(id) {
                self.exclude.insert(id.clone());
            }
            Ok(false)
        } else {
            self.exclude.remove(id);
            if !self.auto.contains(id) {
                self.include.insert(id.clone());
            }
            Ok(true)
        }
    }

    /// Checked and above the cutoff: part of the selection.
    pub fn is_checked(&self, id: &CandidateId, facts: &impl Facts) -> bool {
        self.position(id)
            .is_some_and(|position| position < self.cutoff)
            && self.is_marked(id, facts)
    }

    /// The checkbox state regardless of the cutoff. Below the cutoff it is
    /// what the row would get if the cutoff moved past it now.
    pub fn is_marked(&self, id: &CandidateId, facts: &impl Facts) -> bool {
        let Some(position) = self.position(id) else {
            return false;
        };
        let by_cutoff = if position < self.cutoff {
            self.auto.contains(id)
        } else {
            facts.recommended(id)
        };
        facts.available(id)
            && facts.eligible(id)
            && (by_cutoff || self.include.contains(id))
            && !self.exclude.contains(id)
    }

    pub fn selected(&self, facts: &impl Facts) -> Vec<CandidateId> {
        self.order[..self.cutoff]
            .iter()
            .filter(|id| self.is_checked(id, facts))
            .cloned()
            .collect()
    }

    /// Explicit re-sort. Keeps the cutoff position and individual overrides,
    /// and re-applies the cutoff to the new order.
    pub fn rerank(&mut self, facts: &impl Facts) {
        self.sort(facts);
        self.frozen = true;
        self.recompute_auto(facts);
    }

    /// Clears the cutoff and every override, and resumes live ordering.
    pub fn reset(&mut self, facts: &impl Facts) {
        self.frozen = false;
        self.cutoff = 0;
        self.auto.clear();
        self.include.clear();
        self.exclude.clear();
        self.sort(facts);
    }

    fn recompute_auto(&mut self, facts: &impl Facts) {
        self.auto = self.order[..self.cutoff]
            .iter()
            .filter(|id| facts.eligible(id) && facts.recommended(id))
            .cloned()
            .collect();
    }

    fn sort(&mut self, facts: &impl Facts) {
        self.order.sort_by(|a, b| {
            facts
                .rank(b)
                .partial_cmp(&facts.rank(a))
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
        self.reindex();
    }

    fn reindex(&mut self) {
        self.positions = self
            .order
            .iter()
            .enumerate()
            .map(|(position, id)| (id.clone(), position))
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;
    use crate::model::Registration;

    #[derive(Default)]
    struct Fake {
        rank: RefCell<HashMap<CandidateId, f64>>,
        ineligible: RefCell<HashSet<CandidateId>>,
        unrecommended: RefCell<HashSet<CandidateId>>,
        unavailable: RefCell<HashSet<CandidateId>>,
    }

    impl Facts for Fake {
        fn rank(&self, id: &CandidateId) -> f64 {
            self.rank
                .borrow()
                .get(id)
                .copied()
                .unwrap_or(f64::NEG_INFINITY)
        }
        fn eligible(&self, id: &CandidateId) -> bool {
            !self.ineligible.borrow().contains(id)
        }
        fn recommended(&self, id: &CandidateId) -> bool {
            !self.unrecommended.borrow().contains(id)
        }
        fn available(&self, id: &CandidateId) -> bool {
            !self.unavailable.borrow().contains(id)
        }
    }

    fn id(name: &str) -> CandidateId {
        CandidateId {
            repo: PathBuf::from("/r/.git"),
            registration: Registration::Linked(name.into()),
        }
    }

    fn setup(ranks: &[(&str, f64)]) -> (Selection, Fake) {
        let fake = Fake::default();
        let mut selection = Selection::default();
        for (name, rank) in ranks {
            fake.rank.borrow_mut().insert(id(name), *rank);
            selection.insert(id(name), &fake);
        }
        (selection, fake)
    }

    fn names(ids: &[CandidateId]) -> Vec<String> {
        ids.iter()
            .map(|id| match &id.registration {
                Registration::Linked(name) => name.clone(),
                Registration::Main => "@main".into(),
            })
            .collect()
    }

    #[test]
    fn nothing_selected_until_cutoff_moves() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        assert!(selection.selected(&fake).is_empty());
        assert!(!selection.is_frozen());
        selection.move_cutoff(2, &fake);
        assert_eq!(names(&selection.selected(&fake)), ["a", "b"]);
    }

    #[test]
    fn cutoff_selects_only_eligible_recommended_rows_and_keeps_exclusions() {
        let (mut selection, fake) = setup(&[("a", 4.0), ("b", 3.0), ("c", 2.0), ("d", 1.0)]);
        fake.ineligible.borrow_mut().insert(id("b"));
        fake.unrecommended.borrow_mut().insert(id("c"));
        selection.set_cutoff(4, &fake);
        assert_eq!(names(&selection.selected(&fake)), ["a", "d"]);

        assert_eq!(selection.toggle(&id("a"), &fake), Ok(false));
        assert_eq!(selection.toggle(&id("c"), &fake), Ok(true));
        assert_eq!(
            selection.toggle(&id("b"), &fake),
            Err(ToggleError::NotEligible)
        );
        assert_eq!(names(&selection.selected(&fake)), ["c", "d"]);

        // Individual choices survive cutoff movement, disabled while below.
        selection.set_cutoff(1, &fake);
        assert!(selection.selected(&fake).is_empty());
        assert!(selection.is_marked(&id("c"), &fake));
        assert!(!selection.is_marked(&id("a"), &fake));
        selection.set_cutoff(4, &fake);
        assert_eq!(names(&selection.selected(&fake)), ["c", "d"]);
    }

    #[test]
    fn rows_below_cutoff_show_their_state_but_are_disabled() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        fake.unrecommended.borrow_mut().insert(id("c"));
        selection.set_cutoff(1, &fake);
        assert!(selection.is_marked(&id("b"), &fake));
        assert!(!selection.is_marked(&id("c"), &fake));
        assert_eq!(names(&selection.selected(&fake)), ["a"]);
        assert_eq!(
            selection.toggle(&id("b"), &fake),
            Err(ToggleError::BelowCutoff)
        );
    }

    #[test]
    fn background_updates_do_not_change_selection_or_order() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        fake.ineligible.borrow_mut().insert(id("b"));
        selection.set_cutoff(2, &fake);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);

        // b becomes eligible and c outranks everything: neither is selected
        // or moved until the user acts.
        fake.ineligible.borrow_mut().clear();
        fake.rank.borrow_mut().insert(id("c"), 10.0);
        selection.refresh(&fake);
        assert_eq!(names(selection.order()), ["a", "b", "c"]);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);

        // New candidates join at the end, below the cutoff.
        fake.rank.borrow_mut().insert(id("z"), 100.0);
        selection.insert(id("z"), &fake);
        assert_eq!(names(selection.order()), ["a", "b", "c", "z"]);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);
    }

    #[test]
    fn rerank_reapplies_cutoff_and_preserves_exclusions() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        selection.set_cutoff(2, &fake);
        selection.toggle(&id("a"), &fake).unwrap();
        fake.rank.borrow_mut().insert(id("c"), 10.0);
        selection.rerank(&fake);
        assert_eq!(names(selection.order()), ["c", "a", "b"]);
        assert_eq!(names(&selection.selected(&fake)), ["c"]);
    }

    #[test]
    fn queued_rows_drop_out_and_removal_shifts_cutoff() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        selection.set_cutoff(2, &fake);
        fake.unavailable.borrow_mut().insert(id("a"));
        assert_eq!(names(&selection.selected(&fake)), ["b"]);
        assert_eq!(
            selection.toggle(&id("a"), &fake),
            Err(ToggleError::Unavailable)
        );

        selection.remove(&id("a"));
        assert_eq!(selection.cutoff(), 1);
        assert_eq!(names(&selection.selected(&fake)), ["b"]);
    }

    #[test]
    fn reset_clears_everything_and_resumes_live_order() {
        let (mut selection, fake) = setup(&[("a", 2.0), ("b", 1.0)]);
        selection.set_cutoff(2, &fake);
        selection.toggle(&id("a"), &fake).unwrap();
        selection.reset(&fake);
        assert!(!selection.is_frozen());
        assert!(!selection.is_excluded(&id("a")));
        assert!(selection.selected(&fake).is_empty());
    }
}
