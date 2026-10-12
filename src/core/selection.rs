//! The ranked list, its cutoff, and per-row overrides.
//!
//! A row is marked when it is eligible and either recommended or explicitly
//! included, and not explicitly excluded. Marks are computed live from the
//! current facts. Overrides remember the decision revision they were made
//! against and lapse when the facts change, so a choice never outlives the
//! evidence it was based on. Rows above the cutoff that are marked are
//! selected; rows below it show their would-be mark but are disabled.

use std::collections::HashMap;

use crate::core::facts::DecisionRevision;
use crate::core::ids::WorktreeKey;

pub trait Facts {
    /// Higher ranks first. Unrated candidates should return `f64::NEG_INFINITY`.
    fn rank(&self, key: &WorktreeKey) -> f64;
    fn eligible(&self, key: &WorktreeKey) -> bool;
    fn recommended(&self, key: &WorktreeKey) -> bool;
    /// False while queued, running, or removed.
    fn available(&self, key: &WorktreeKey) -> bool;
    fn revision(&self, key: &WorktreeKey) -> Option<DecisionRevision>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleError {
    BelowCutoff,
    NotEligible,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Override {
    include: bool,
    revision: DecisionRevision,
}

/// What the user approved on submit: each row with the revision it was
/// shown at. Validation refuses rows whose facts moved on.
pub type Snapshot = Vec<(WorktreeKey, DecisionRevision)>;

#[derive(Debug, Default)]
pub struct Selection {
    order: Vec<WorktreeKey>,
    positions: HashMap<WorktreeKey, usize>,
    /// The order is held still once the user acts on it.
    frozen: bool,
    cutoff: usize,
    overrides: HashMap<WorktreeKey, Override>,
}

impl Selection {
    pub fn order(&self) -> &[WorktreeKey] {
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

    pub fn position(&self, key: &WorktreeKey) -> Option<usize> {
        self.positions.get(key).copied()
    }

    /// Appends a new row; `refresh` sorts it in unless the order is frozen,
    /// in which case it stays at the end.
    pub fn push(&mut self, key: WorktreeKey) {
        if self.positions.contains_key(&key) {
            return;
        }
        self.positions.insert(key.clone(), self.order.len());
        self.order.push(key);
    }

    /// Re-sorts after rating changes unless frozen, and drops overrides
    /// whose facts changed.
    pub fn refresh(&mut self, facts: &impl Facts) {
        self.overrides
            .retain(|key, choice| facts.revision(key) == Some(choice.revision));
        if !self.frozen {
            self.sort(facts);
        }
    }

    pub fn remove(&mut self, key: &WorktreeKey) {
        let Some(position) = self.positions.remove(key) else {
            return;
        };
        self.order.remove(position);
        if position < self.cutoff {
            self.cutoff -= 1;
        }
        self.overrides.remove(key);
        self.reindex();
    }

    pub fn set_cutoff(&mut self, cutoff: usize) {
        self.frozen = true;
        self.cutoff = cutoff.min(self.order.len());
    }

    pub fn move_cutoff(&mut self, delta: isize) {
        self.set_cutoff(self.cutoff.saturating_add_signed(delta));
    }

    /// Flips the row's mark and returns whether it is now marked.
    pub fn toggle(&mut self, key: &WorktreeKey, facts: &impl Facts) -> Result<bool, ToggleError> {
        let Some(position) = self.position(key) else {
            return Err(ToggleError::Unavailable);
        };
        if position >= self.cutoff {
            return Err(ToggleError::BelowCutoff);
        }
        if !facts.available(key) {
            return Err(ToggleError::Unavailable);
        }
        if !facts.eligible(key) {
            return Err(ToggleError::NotEligible);
        }
        let Some(revision) = facts.revision(key) else {
            return Err(ToggleError::Unavailable);
        };
        self.frozen = true;
        let include = !self.is_marked(key, facts);
        if include == facts.recommended(key) {
            self.overrides.remove(key);
        } else {
            self.overrides
                .insert(key.clone(), Override { include, revision });
        }
        Ok(include)
    }

    /// The checkbox state regardless of the cutoff.
    pub fn is_marked(&self, key: &WorktreeKey, facts: &impl Facts) -> bool {
        if !(facts.available(key) && facts.eligible(key)) {
            return false;
        }
        let revision = facts.revision(key);
        match self.overrides.get(key) {
            Some(choice) if Some(choice.revision) == revision => choice.include,
            _ => facts.recommended(key),
        }
    }

    /// Marked and above the cutoff: part of the selection.
    pub fn is_checked(&self, key: &WorktreeKey, facts: &impl Facts) -> bool {
        self.position(key)
            .is_some_and(|position| position < self.cutoff)
            && self.is_marked(key, facts)
    }

    pub fn selected(&self, facts: &impl Facts) -> Vec<WorktreeKey> {
        self.order[..self.cutoff]
            .iter()
            .filter(|key| self.is_marked(key, facts))
            .cloned()
            .collect()
    }

    /// The selection with the revision each row is approved at.
    pub fn snapshot(&self, facts: &impl Facts) -> Snapshot {
        self.selected(facts)
            .into_iter()
            .filter_map(|key| Some((key.clone(), facts.revision(&key)?)))
            .collect()
    }

    /// Explicit re-sort; keeps the cutoff position and overrides.
    pub fn rerank(&mut self, facts: &impl Facts) {
        self.sort(facts);
        self.frozen = true;
    }

    /// Clears the cutoff and every override, and resumes live ordering.
    pub fn reset(&mut self, facts: &impl Facts) {
        self.frozen = false;
        self.cutoff = 0;
        self.overrides.clear();
        self.sort(facts);
    }

    fn sort(&mut self, facts: &impl Facts) {
        self.order.sort_by(|a, b| {
            facts
                .rank(b)
                .total_cmp(&facts.rank(a))
                .then_with(|| a.cmp(b))
        });
        self.reindex();
    }

    fn reindex(&mut self) {
        self.positions = self
            .order
            .iter()
            .enumerate()
            .map(|(position, key)| (key.clone(), position))
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;
    use crate::core::ids::RepoKey;

    #[derive(Default)]
    struct Fake {
        rank: RefCell<HashMap<WorktreeKey, f64>>,
        ineligible: RefCell<HashSet<WorktreeKey>>,
        unrecommended: RefCell<HashSet<WorktreeKey>>,
        unavailable: RefCell<HashSet<WorktreeKey>>,
        revision: RefCell<HashMap<WorktreeKey, u64>>,
    }

    impl Facts for Fake {
        fn rank(&self, key: &WorktreeKey) -> f64 {
            self.rank
                .borrow()
                .get(key)
                .copied()
                .unwrap_or(f64::NEG_INFINITY)
        }
        fn eligible(&self, key: &WorktreeKey) -> bool {
            !self.ineligible.borrow().contains(key)
        }
        fn recommended(&self, key: &WorktreeKey) -> bool {
            !self.unrecommended.borrow().contains(key)
        }
        fn available(&self, key: &WorktreeKey) -> bool {
            !self.unavailable.borrow().contains(key)
        }
        fn revision(&self, key: &WorktreeKey) -> Option<DecisionRevision> {
            Some(DecisionRevision(
                self.revision.borrow().get(key).copied().unwrap_or(0),
            ))
        }
    }

    fn key(name: &str) -> WorktreeKey {
        WorktreeKey::linked(&RepoKey::new("/r/.git"), name)
    }

    fn setup(ranks: &[(&str, f64)]) -> (Selection, Fake) {
        let fake = Fake::default();
        let mut selection = Selection::default();
        for (name, rank) in ranks {
            fake.rank.borrow_mut().insert(key(name), *rank);
            selection.push(key(name));
        }
        selection.refresh(&fake);
        (selection, fake)
    }

    fn names(keys: &[WorktreeKey]) -> Vec<String> {
        keys.iter()
            .map(|key| key.admin_id.as_ref().unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn nothing_selected_until_cutoff_moves() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        assert!(selection.selected(&fake).is_empty());
        selection.move_cutoff(2);
        assert_eq!(names(&selection.selected(&fake)), ["a", "b"]);
    }

    #[test]
    fn overrides_apply_above_cutoff_and_survive_cutoff_moves() {
        let (mut selection, fake) = setup(&[("a", 4.0), ("b", 3.0), ("c", 2.0), ("d", 1.0)]);
        fake.ineligible.borrow_mut().insert(key("b"));
        fake.unrecommended.borrow_mut().insert(key("c"));
        selection.set_cutoff(4);
        assert_eq!(names(&selection.selected(&fake)), ["a", "d"]);

        assert_eq!(selection.toggle(&key("a"), &fake), Ok(false));
        assert_eq!(selection.toggle(&key("c"), &fake), Ok(true));
        assert_eq!(
            selection.toggle(&key("b"), &fake),
            Err(ToggleError::NotEligible)
        );
        assert_eq!(names(&selection.selected(&fake)), ["c", "d"]);

        selection.set_cutoff(1);
        assert!(selection.selected(&fake).is_empty());
        assert!(selection.is_marked(&key("c"), &fake));
        assert_eq!(
            selection.toggle(&key("c"), &fake),
            Err(ToggleError::BelowCutoff)
        );
        selection.set_cutoff(4);
        assert_eq!(names(&selection.selected(&fake)), ["c", "d"]);
    }

    #[test]
    fn overrides_lapse_when_the_facts_change() {
        let (mut selection, fake) = setup(&[("a", 2.0), ("b", 1.0)]);
        fake.unrecommended.borrow_mut().insert(key("b"));
        selection.set_cutoff(2);
        selection.toggle(&key("b"), &fake).unwrap();
        assert_eq!(names(&selection.selected(&fake)), ["a", "b"]);

        // b got a new commit: the inclusion was made against old facts.
        fake.revision.borrow_mut().insert(key("b"), 1);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);
        selection.refresh(&fake);
        fake.revision.borrow_mut().insert(key("b"), 0);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);
    }

    #[test]
    fn marks_follow_live_facts_and_order_stays_frozen() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        fake.ineligible.borrow_mut().insert(key("b"));
        selection.set_cutoff(2);
        assert_eq!(names(&selection.selected(&fake)), ["a"]);

        fake.ineligible.borrow_mut().clear();
        fake.unrecommended.borrow_mut().insert(key("a"));
        fake.rank.borrow_mut().insert(key("c"), 10.0);
        selection.refresh(&fake);
        assert_eq!(names(selection.order()), ["a", "b", "c"]);
        assert_eq!(names(&selection.selected(&fake)), ["b"]);

        selection.rerank(&fake);
        assert_eq!(names(selection.order()), ["c", "a", "b"]);
    }

    #[test]
    fn nan_ranks_sort_deterministically() {
        let (selection, _) = setup(&[("a", f64::NAN), ("b", 1.0), ("c", 2.0)]);
        assert_eq!(names(selection.order()), ["a", "c", "b"]);
    }

    #[test]
    fn snapshot_carries_revisions_and_removal_shifts_cutoff() {
        let (mut selection, fake) = setup(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        fake.revision.borrow_mut().insert(key("b"), 7);
        selection.set_cutoff(2);
        assert_eq!(
            selection.snapshot(&fake),
            vec![
                (key("a"), DecisionRevision(0)),
                (key("b"), DecisionRevision(7))
            ]
        );
        fake.unavailable.borrow_mut().insert(key("a"));
        assert_eq!(names(&selection.selected(&fake)), ["b"]);
        selection.remove(&key("a"));
        assert_eq!(selection.cutoff(), 1);
    }
}
