//! Cleanup UI state: one ranked list with a movable cutoff. Pure state and
//! key handling; the engine is reached through `Control`, the screen
//! through `view::gc`.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::core::format;
use crate::core::ids::{RepoKey, WorktreeKey};
use crate::core::selection::ToggleError;
use crate::removal::request::{Outcome, Request};
use crate::scan::engine::{CancelResult, Engine, Event, RemovalState};
use crate::scan::model::Model;

use super::event_loop::{App, Flow};
use super::input::Toast;
use super::view;

/// Rows below the cutoff that are measured too, so the next candidates
/// have sizes before the cutoff reaches them.
const MEASURE_BELOW_CUTOFF: usize = 20;

/// What the UI asks of the engine.
pub trait Control {
    fn prioritize(&self, key: &WorktreeKey);
    fn set_wanted(&self, keys: Vec<WorktreeKey>);
    fn set_measure_paused(&self, paused: bool);
    fn rescan(&self, repo: RepoKey);
    fn fetch(&self, repo: RepoKey) -> bool;
    fn submit(&self, requests: Vec<Request>) -> Vec<WorktreeKey>;
    fn cancel(&self, key: &WorktreeKey) -> CancelResult;
    fn stop(&self);
}

impl Control for Engine {
    fn prioritize(&self, key: &WorktreeKey) {
        Engine::prioritize(self, key);
    }
    fn set_wanted(&self, keys: Vec<WorktreeKey>) {
        Engine::set_wanted(self, keys);
    }
    fn set_measure_paused(&self, paused: bool) {
        Engine::set_measure_paused(self, paused);
    }
    fn rescan(&self, repo: RepoKey) {
        Engine::rescan(self, repo);
    }
    fn fetch(&self, repo: RepoKey) -> bool {
        Engine::fetch(self, repo)
    }
    fn submit(&self, requests: Vec<Request>) -> Vec<WorktreeKey> {
        Engine::submit(self, requests)
    }
    fn cancel(&self, key: &WorktreeKey) -> CancelResult {
        Engine::cancel(self, key)
    }
    fn stop(&self) {
        Engine::stop(self);
    }
}

/// How the UI ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// Nothing was left running.
    Clean,
    /// Left while removals were running; they are cut short.
    Forced,
}

/// A visual row of the table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Slot<K = WorktreeKey> {
    /// Above every candidate and the cutoff; focus is `None` here.
    Top,
    Cutoff,
    Row(K),
}

pub struct GcApp<C: Control> {
    pub model: Model,
    pub control: C,
    pub focus: Option<WorktreeKey>,
    pub filter: Input,
    pub editing_filter: bool,
    pub details: bool,
    pub help: bool,
    /// Also list main worktrees and blocked rows, which cannot be removed.
    pub show_all: bool,
    pub toast: Toast,
    /// Startup messages, such as recovered removals.
    pub notices: Vec<String>,
    /// Removed rows hidden by `C`.
    pub compacted: HashSet<WorktreeKey>,
    pub measure_paused: bool,
    pub quitting: bool,
    /// First layout row in view, and what each drawn row holds.
    pub offset: usize,
    pub table_area: Rect,
    pub table_rows: Vec<Slot>,
    pub page: usize,
    delete_now_armed: bool,
    next_generation: u64,
    dragging_cutoff: bool,
    wanted: Vec<WorktreeKey>,
    wanted_set: HashSet<WorktreeKey>,
}

impl<C: Control> GcApp<C> {
    pub fn new(model: Model, control: C) -> Self {
        GcApp {
            model,
            control,
            focus: None,
            filter: Input::default(),
            editing_filter: false,
            details: false,
            help: false,
            show_all: false,
            toast: Toast::default(),
            notices: Vec::new(),
            compacted: HashSet::new(),
            measure_paused: false,
            quitting: false,
            offset: 0,
            table_area: Rect::default(),
            table_rows: Vec::new(),
            page: 10,
            delete_now_armed: false,
            next_generation: 1,
            dragging_cutoff: false,
            wanted: Vec::new(),
            wanted_set: HashSet::new(),
        }
    }

    /// Whether the row's size will be measured without further input.
    pub fn will_measure(&self, key: &WorktreeKey) -> bool {
        self.wanted_set.contains(key) || self.focus.as_ref() == Some(key)
    }

    pub fn has_running(&self) -> bool {
        self.model.rows.values().any(|row| row.is_removal_active())
    }

    fn sync_wanted(&mut self) {
        let selection = &self.model.selection;
        let end = (selection.cutoff() + MEASURE_BELOW_CUTOFF).min(selection.len());
        let wanted = &selection.order()[..end];
        if wanted != self.wanted.as_slice() {
            self.wanted = wanted.to_vec();
            self.wanted_set = wanted.iter().cloned().collect();
            self.control.set_wanted(self.wanted.clone());
        }
    }

    fn path_of(&self, key: &WorktreeKey) -> String {
        self.model.rows.get(key).map_or_else(
            || key.to_string(),
            |row| format::path(&row.candidate.seed.path),
        )
    }

    /// Whether a row is listed: removable, still being assessed, or part
    /// of a removal; the rest only with `show_all`.
    pub fn is_listed(&self, key: &WorktreeKey) -> bool {
        if self.compacted.contains(key) {
            return false;
        }
        self.show_all
            || self.model.rows.get(key).is_some_and(|row| {
                row.removal.is_some()
                    || !matches!(row.eligibility, Err(ref block) if !block.is_pending())
            })
    }

    /// Rows hidden because they cannot be acted on.
    pub fn hidden_count(&self) -> usize {
        self.model
            .selection
            .order()
            .iter()
            .filter(|key| !self.compacted.contains(*key) && !self.is_listed(key))
            .count()
    }

    /// Rows that pass the filter, in list order.
    pub fn visible(&self) -> Vec<WorktreeKey> {
        let terms: Vec<String> = self
            .filter
            .value()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        self.model
            .selection
            .order()
            .iter()
            .filter(|key| self.is_listed(key))
            .filter(|key| {
                terms.is_empty()
                    || self.model.rows.get(*key).is_some_and(|row| {
                        let haystack = format!(
                            "{} {} {} {}",
                            row.candidate.seed.repo_label,
                            row.candidate.head_label(),
                            format::path(&row.candidate.seed.path),
                            view::gc::state_text(row)
                        )
                        .to_lowercase();
                        terms.iter().all(|term| haystack.contains(term))
                    })
            })
            .cloned()
            .collect()
    }

    pub fn focused_in(&self, visible: &[WorktreeKey]) -> Option<WorktreeKey> {
        self.focus.clone().filter(|key| visible.contains(key))
    }

    fn focused(&self) -> Option<WorktreeKey> {
        self.focused_in(&self.visible())
    }

    fn set_focus(&mut self, key: Option<WorktreeKey>) {
        if let Some(key) = &key {
            self.control.prioritize(key);
        }
        self.focus = key;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Flow<Exit> {
        self.notices.clear();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.delete_now_armed = false;
            return self.request_quit(true);
        }
        if self.editing_filter {
            match key.code {
                KeyCode::Esc => {
                    self.filter.reset();
                    self.editing_filter = false;
                }
                KeyCode::Enter => self.editing_filter = false,
                _ => {
                    self.filter.handle_event(&TermEvent::Key(key));
                }
            }
            return Flow::Continue;
        }

        let armed = std::mem::take(&mut self.delete_now_armed);
        let half = self.page as isize / 2;
        match key.code {
            KeyCode::Char('q') => return self.request_quit(false),
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(-1),
            KeyCode::Char('d') if ctrl => self.move_focus(half),
            KeyCode::Char('u') if ctrl => self.move_focus(-half),
            KeyCode::PageDown => self.move_focus(self.page as isize),
            KeyCode::PageUp => self.move_focus(-(self.page as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.move_focus(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_focus(isize::MAX / 2),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('J') => self.move_cutoff(1),
            KeyCode::Char('K') => self.move_cutoff(-1),
            KeyCode::Char('=') => self.cutoff_to_focus(),
            KeyCode::Char('D') => self.submit_selected(),
            KeyCode::Char('x') if armed => self.delete_now(),
            KeyCode::Char('x') => self.arm_delete_now(),
            KeyCode::Char('c') => self.cancel_focused(),
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Esc => self.filter.reset(),
            KeyCode::Char('r') => {
                self.model.selection.rerank(&self.model.rows);
                self.toast
                    .say("reranked; review the selection before submitting");
            }
            KeyCode::Char('R') => {
                self.model.selection.reset(&self.model.rows);
                self.toast.say("selection reset");
            }
            KeyCode::Enter | KeyCode::Tab => self.details = !self.details,
            KeyCode::Char('f') => self.fetch_focused(),
            KeyCode::Char('C') => self.compact(),
            KeyCode::Char('p') => self.toggle_measure_pause(),
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.toast.say(if self.show_all {
                    "showing main worktrees and blocked rows too"
                } else {
                    "hiding rows that cannot be removed (a to show)"
                });
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Quits at once when nothing is running; otherwise stops new work and
    /// waits for running removals, unless forced a second time.
    fn request_quit(&mut self, force: bool) -> Flow<Exit> {
        if !self.has_running() {
            return Flow::Exit(Exit::Clean);
        }
        if force && self.quitting {
            return Flow::Exit(Exit::Forced);
        }
        self.quitting = true;
        self.control.stop();
        self.toast.say(
            "stopping: queued removals are cancelled, running ones finish (ctrl-c again to force)",
        );
        Flow::Continue
    }

    fn move_focus(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.focus = None;
            return;
        }
        // Cursor positions: 0 is the top slot, i + 1 is visible[i].
        let current = self
            .focused_in(&visible)
            .and_then(|key| visible.iter().position(|other| *other == key))
            .map_or(0, |index| index + 1);
        let next = current.saturating_add_signed(delta).min(visible.len());
        self.set_focus((next > 0).then(|| visible[next - 1].clone()));
    }

    fn toggle(&mut self) {
        let Some(key) = self.focused() else { return };
        match self.model.selection.toggle(&key, &self.model.rows) {
            Ok(_) => {}
            Err(ToggleError::NotEligible) => {
                let reason = self.model.rows[&key]
                    .eligibility
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                self.toast.say(format!("not removable: {reason}"));
            }
            Err(ToggleError::Unavailable) => self.toast.say("already queued or removed"),
        }
    }

    fn cutoff_blocked_by_filter(&mut self) -> bool {
        let blocked = !self.filter.value().is_empty();
        if blocked {
            self.toast.say("clear the filter (esc) to move the cutoff");
        }
        blocked
    }

    fn move_cutoff(&mut self, delta: isize) {
        if self.cutoff_blocked_by_filter() {
            return;
        }
        self.model.selection.move_cutoff(delta);
        // The cursor travels with the line.
        let selection = &self.model.selection;
        let focus = selection
            .cutoff()
            .checked_sub(1)
            .and_then(|index| selection.order().get(index).cloned());
        self.set_focus(focus);
    }

    fn cutoff_to_focus(&mut self) {
        if self.cutoff_blocked_by_filter() {
            return;
        }
        let cutoff = self
            .focused()
            .and_then(|key| self.model.selection.position(&key))
            .map_or(0, |position| position + 1);
        self.model.selection.set_cutoff(cutoff);
    }

    /// Queues the rows of `snapshot` whose facts still match it.
    fn submit(
        &mut self,
        snapshot: Vec<(WorktreeKey, crate::core::facts::DecisionRevision)>,
    ) -> usize {
        let generation = self.next_generation;
        self.next_generation += 1;
        let requests: Vec<Request> = snapshot
            .into_iter()
            .filter_map(|(key, revision)| {
                let row = self.model.rows.get(&key)?;
                let request = Request::from_candidate(&row.candidate, generation)?;
                (row.eligibility.is_ok() && request.revision == revision).then_some(request)
            })
            .collect();
        let accepted = self.control.submit(requests);
        for key in &accepted {
            self.model
                .set_removal(key, RemovalState::Queued { batch: generation });
        }
        self.model.selection.refresh(&self.model.rows);
        accepted.len()
    }

    fn submit_selected(&mut self) {
        let snapshot = self.model.selection.snapshot(&self.model.rows);
        if snapshot.is_empty() {
            self.toast
                .say("nothing checked: space checks a row, J moves the line down");
            return;
        }
        let visible: HashSet<WorktreeKey> = self.visible().into_iter().collect();
        let hidden = snapshot
            .iter()
            .filter(|(key, _)| !visible.contains(key))
            .count();
        let batch = self.next_generation;
        let queued = self.submit(snapshot);
        let hidden_note = if hidden > 0 {
            format!(" ({hidden} hidden by the filter)")
        } else {
            String::new()
        };
        self.toast
            .say(format!("batch {batch}: {queued} queued{hidden_note}"));
    }

    fn arm_delete_now(&mut self) {
        let Some(key) = self.focused() else { return };
        let row = &self.model.rows[&key];
        if let Some(state) = &row.removal
            && !matches!(state, RemovalState::Finished(outcome) if outcome.allows_retry())
        {
            let text = view::gc::removal_text(state);
            self.toast.say(format!("already {text}"));
            return;
        }
        if let Err(block) = &row.eligibility {
            self.toast.say(format!("not removable: {block}"));
            return;
        }
        self.delete_now_armed = true;
        self.toast.say(format!(
            "press x again to delete {} now",
            format::path(&row.candidate.seed.path)
        ));
    }

    fn delete_now(&mut self) {
        let Some(key) = self.focused() else { return };
        let path = self.path_of(&key);
        let Some(row) = self.model.rows.get(&key) else {
            return;
        };
        let snapshot = vec![(key, row.candidate.revision())];
        if self.submit(snapshot) == 1 {
            self.toast.say(format!("deleting {path}"));
        } else {
            self.toast.say(format!("{path} is already queued"));
        }
    }

    fn cancel_focused(&mut self) {
        let Some(key) = self.focused() else { return };
        match self.control.cancel(&key) {
            CancelResult::Cancelled => {
                self.model
                    .set_removal(&key, RemovalState::Finished(Outcome::Cancelled));
                self.model.selection.refresh(&self.model.rows);
                self.toast.say("cancelled");
            }
            CancelResult::Requested => self.toast.say("cancelling before removal starts"),
            CancelResult::Running => self
                .toast
                .say("removal already running; it cannot be cancelled"),
            CancelResult::NotQueued => self.toast.say("nothing queued for this row"),
        }
    }

    fn fetch_focused(&mut self) {
        let Some(key) = self.focused() else { return };
        let label = self.repo_label(&key.repo());
        if self.control.fetch(key.repo()) {
            self.toast.say(format!("fetching {label}"));
        } else {
            self.toast.say(format!("already fetching {label}"));
        }
    }

    fn repo_label(&self, repo: &RepoKey) -> String {
        self.model
            .rows
            .values()
            .find(|row| row.candidate.key().repo() == *repo)
            .map_or_else(
                || repo.to_string(),
                |row| row.candidate.seed.repo_label.clone(),
            )
    }

    fn compact(&mut self) {
        let removed: Vec<WorktreeKey> = self
            .model
            .rows
            .iter()
            .filter(|(_, row)| row.is_removed())
            .map(|(key, _)| key.clone())
            .collect();
        let count = removed.len();
        self.compacted.extend(removed);
        self.toast.say(format!("hid {count} removed rows"));
    }

    fn toggle_measure_pause(&mut self) {
        self.measure_paused = !self.measure_paused;
        self.control.set_measure_paused(self.measure_paused);
        self.toast.say(if self.measure_paused {
            "size measurement paused (p to resume)"
        } else {
            "size measurement resumed"
        });
    }

    /// Visual table row under a screen position, below the header.
    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let area = self.table_area;
        let inside = column >= area.x
            && column < area.x + area.width
            && row > area.y
            && row < area.y + area.height;
        let index = usize::from(row.checked_sub(area.y + 1)?);
        (inside && index < self.table_rows.len()).then_some(index)
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        self.notices.clear();
        self.delete_now_armed = false;
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_focus(3),
            MouseEventKind::ScrollUp => self.move_focus(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(index) = self.row_at(mouse.column, mouse.row) else {
                    return;
                };
                match self.table_rows[index].clone() {
                    Slot::Top => self.focus = None,
                    Slot::Cutoff => {
                        if !self.cutoff_blocked_by_filter() {
                            self.dragging_cutoff = true;
                        }
                    }
                    Slot::Row(key) => {
                        self.set_focus(Some(key));
                        // The checkbox column is the first three cells.
                        if mouse.column < self.table_area.x + 3 {
                            self.toggle();
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_cutoff => {
                self.drag_cutoff_to(mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging_cutoff = false,
            _ => {}
        }
    }

    /// The separator follows the pointer: below the hovered row when
    /// dragging down, above it when dragging up.
    fn drag_cutoff_to(&mut self, screen_row: u16) {
        let area = self.table_area;
        let last = area.y + area.height.saturating_sub(1);
        let clamped = screen_row.clamp(area.y + 1, last.max(area.y + 1));
        let Some(index) = self.row_at(area.x, clamped) else {
            return;
        };
        let cutoff = match &self.table_rows[index] {
            Slot::Top => 0,
            Slot::Cutoff => return,
            Slot::Row(key) => {
                let Some(position) = self.model.selection.position(key) else {
                    return;
                };
                if position < self.model.selection.cutoff() {
                    position
                } else {
                    position + 1
                }
            }
        };
        if cutoff != self.model.selection.cutoff() {
            self.model.selection.set_cutoff(cutoff);
        }
    }

    pub fn apply(&mut self, event: Event) {
        if let Event::Fetched { repo, error } = &event {
            let label = self.repo_label(repo);
            self.toast.say(match error {
                None => format!("fetched {label}"),
                Some(error) => format!("fetching {label} failed: {error}"),
            });
        }
        if let Some((key, outcome)) = self.model.apply(event) {
            if matches!(outcome, Outcome::Blocked { .. } | Outcome::Failed { .. }) {
                // A retry needs fresh evidence.
                self.control.rescan(key.repo());
            }
            if !matches!(outcome, Outcome::Cancelled) && !outcome.is_clean_success() {
                self.toast.say(format!("{}: {outcome}", self.path_of(&key)));
            }
        }
    }
}

impl<C: Control> App for GcApp<C> {
    type Event = Event;
    type Output = Exit;

    fn on_input(&mut self, event: TermEvent) -> Flow<Exit> {
        match event {
            TermEvent::Key(key) => self.on_key(key),
            TermEvent::Mouse(mouse) => {
                self.on_mouse(mouse);
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }

    fn on_event(&mut self, event: Event) {
        self.apply(event);
    }

    fn on_tick(&mut self) {
        self.model.tick();
    }

    fn on_signal(&mut self, repeated: bool) -> Flow<Exit> {
        self.request_quit(repeated)
    }

    fn finished(&mut self) -> Option<Exit> {
        (self.quitting && !self.has_running()).then_some(Exit::Clean)
    }

    fn settle(&mut self) {
        self.model.settle();
        self.sync_wanted();
    }

    fn draw(&mut self, frame: &mut Frame) {
        view::gc::draw(self, frame);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;

    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use crate::core::candidate::{Kind, Seed};
    use crate::core::config::Config;
    use crate::core::facts::{
        BaseRef, DirState, GitFacts, Head, Integration, StatusEvidence, StorageFacts,
    };
    use crate::core::ids::YardPath;
    use crate::core::obs::Obs;

    #[derive(Default)]
    struct Fake {
        submitted: RefCell<Vec<Request>>,
        stopped: RefCell<bool>,
    }

    impl Control for &Fake {
        fn prioritize(&self, _: &WorktreeKey) {}
        fn set_wanted(&self, _: Vec<WorktreeKey>) {}
        fn set_measure_paused(&self, _: bool) {}
        fn rescan(&self, _: RepoKey) {}
        fn fetch(&self, _: RepoKey) -> bool {
            true
        }
        fn submit(&self, requests: Vec<Request>) -> Vec<WorktreeKey> {
            let keys = requests.iter().map(|request| request.key.clone()).collect();
            self.submitted.borrow_mut().extend(requests);
            keys
        }
        fn cancel(&self, _: &WorktreeKey) -> CancelResult {
            CancelResult::NotQueued
        }
        fn stop(&self) {
            *self.stopped.borrow_mut() = true;
        }
    }

    fn key(name: &str) -> WorktreeKey {
        WorktreeKey::linked(&RepoKey::new("/r/.git"), name)
    }

    fn row(app: &mut GcApp<&Fake>, name: &str, oid: &str) {
        let key = key(name);
        app.apply(Event::Seed(Seed {
            key: key.clone(),
            kind: Kind::LinkedWorktree,
            path: YardPath::new(format!("/r.{name}")),
            repo_label: "r".into(),
            admin_dir: YardPath::new(format!("/r/.git/worktrees/{name}")),
            lock: None,
            dir: DirState::Present { dev: 1, ino: 2 },
            git_activity: Some(0),
            registration_error: None,
            conflict: None,
            fetched_at: None,
        }));
        app.apply(Event::Git {
            key: key.clone(),
            observation: Obs::Complete {
                value: GitFacts {
                    head: Head::Branch {
                        name: name.into(),
                        oid: oid.into(),
                    },
                    last_commit: Some(0),
                    subject: None,
                    integration: Integration::Merged {
                        into: "main".into(),
                    },
                    status: Some(StatusEvidence::default()),
                    unreachable_commits: false,
                    in_progress: None,
                    private_modules: false,
                    bases: vec![BaseRef {
                        name: "main".into(),
                        oid: "b".into(),
                        branch: "main".into(),
                    }],
                },
                observed_at: 0,
            },
            watermark: Some(0),
        });
        app.apply(Event::Storage {
            key,
            observation: Obs::Complete {
                value: StorageFacts::default(),
                observed_at: 0,
            },
        });
        app.settle();
    }

    fn press(app: &mut GcApp<&Fake>, code: KeyCode) -> Flow<Exit> {
        app.on_key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn submission_carries_the_revision_shown_and_a_new_generation() {
        let fake = Fake::default();
        let mut app = GcApp::new(Model::new(Arc::new(Config::default())), &fake);
        row(&mut app, "a", "1");
        row(&mut app, "b", "2");
        press(&mut app, KeyCode::Char('J'));
        press(&mut app, KeyCode::Char('J'));
        press(&mut app, KeyCode::Char('D'));
        let submitted = fake.submitted.borrow();
        assert_eq!(submitted.len(), 2);
        assert!(submitted.iter().all(|request| request.generation == 1));
        let shown = app.model.rows[&key("a")].candidate.revision();
        assert!(submitted.iter().any(|request| request.revision == shown));
        // Queued rows leave the selection.
        assert!(app.model.selection.selected(&app.model.rows).is_empty());
    }

    #[test]
    fn quitting_waits_for_running_removals() {
        let fake = Fake::default();
        let mut app = GcApp::new(Model::new(Arc::new(Config::default())), &fake);
        row(&mut app, "a", "1");
        assert!(matches!(
            press(&mut app, KeyCode::Char('q')),
            Flow::Exit(Exit::Clean)
        ));

        app.apply(Event::Removal {
            key: key("a"),
            state: RemovalState::Removing,
        });
        assert!(matches!(
            press(&mut app, KeyCode::Char('q')),
            Flow::Continue
        ));
        assert!(*fake.stopped.borrow());
        assert_eq!(app.finished(), None);
        assert!(matches!(app.on_signal(true), Flow::Exit(Exit::Forced)));
        app.apply(Event::Removal {
            key: key("a"),
            state: RemovalState::Finished(Outcome::Removed {
                branch: None,
                warnings: vec![],
            }),
        });
        assert_eq!(app.finished(), Some(Exit::Clean));
    }
}
