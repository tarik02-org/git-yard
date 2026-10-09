//! Terminal UI: one flat ranked list with a movable cutoff. Scanning and
//! deletion run in the background; the UI thread only applies events and
//! handles input.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, select, tick, unbounded};
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;

use git_yard::config::Config;
use git_yard::discovery::display_path;
use git_yard::engine::{CancelResult, DeleteState, Engine, Event, Measure, Options};
use git_yard::model::{Candidate, CandidateId};
use git_yard::remove::{Outcome, Request};
use git_yard::selection::ToggleError;
use git_yard::state::Model;
use git_yard::store::{Cache, Interrupted, Journal, Paths};
use view::{delete_text, state_text};

/// Upper bound on engine events applied in one go, so input is checked
/// between bursts.
const EVENTS_PER_BATCH: usize = 5000;
/// Minimum time between two redraws; input is still handled immediately.
const FRAME: Duration = Duration::from_millis(16);
const MESSAGE_TTL: Duration = Duration::from_secs(6);
/// Rows below the cutoff that are measured too, so the next candidates
/// have sizes before the cutoff reaches them. Rows further down are only
/// measured when focused.
const MEASURE_BELOW_CUTOFF: usize = 20;

pub fn run(
    config: Arc<Config>,
    journal: Arc<Journal>,
    cached: Vec<Candidate>,
    interrupted: Vec<Interrupted>,
    paths: &Paths,
) -> Result<()> {
    let options = Options {
        measure: Measure::OnDemand,
    };
    let (engine, events) = Engine::start(config.clone(), journal, cached, options);
    let mut app = App::new(Model::new(config), engine);
    app.notices = interrupted.iter().map(Interrupted::describe).collect();

    let input = spawn_input();
    let ticker = tick(Duration::from_secs(1));
    let mut terminal = ratatui::init();
    enable_mouse()?;
    let result = (|| -> Result<()> {
        let mut dirty = true;
        let mut last_draw = Instant::now() - FRAME;
        loop {
            if dirty && last_draw.elapsed() >= FRAME {
                app.model.settle();
                app.sync_wanted();
                terminal.draw(|frame| app.draw(frame))?;
                last_draw = Instant::now();
                dirty = false;
            }
            let wait = if dirty {
                FRAME.saturating_sub(last_draw.elapsed())
            } else {
                Duration::from_secs(1)
            };
            select! {
                recv(input) -> input => {
                    // Key handlers read the order, so pending events are settled first.
                    app.model.settle();
                    match input {
                        Ok(TermEvent::Key(key)) if key.kind == KeyEventKind::Press && app.on_key(key) => break,
                        Ok(TermEvent::Mouse(mouse)) => app.on_mouse(mouse),
                        _ => {}
                    }
                    dirty = true;
                }
                recv(events) -> event => {
                    let Ok(event) = event else { break };
                    app.on_event(event);
                    for event in events.try_iter().take(EVENTS_PER_BATCH) {
                        app.on_event(event);
                    }
                    dirty = true;
                }
                recv(ticker) -> _ => {
                    app.model.tick();
                    dirty = true;
                }
                default(wait) => {}
            }
            if app.quitting && !app.has_running() {
                break;
            }
        }
        Ok(())
    })();
    disable_mouse();
    ratatui::restore();

    let fresh = app
        .model
        .rows
        .values()
        .filter(|row| row.fresh && !row.is_removed())
        .map(|row| row.candidate.clone());
    if let Err(error) = Cache::save(&paths.cache_file, fresh) {
        eprintln!("git-yard: saving cache: {error:#}");
    }
    app.engine.shutdown();
    result
}

fn enable_mouse() -> Result<()> {
    execute!(std::io::stdout(), EnableMouseCapture)?;
    // ratatui's panic hook restores the terminal but not mouse reporting.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse();
        previous(info);
    }));
    Ok(())
}

fn disable_mouse() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

fn spawn_input() -> Receiver<TermEvent> {
    let (sender, receiver) = unbounded();
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if sender.send(event).is_err() {
                return;
            }
        }
    });
    receiver
}

#[derive(Clone, Copy)]
enum Pin {
    None,
    Above,
    Below,
}

/// A visual row of the table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot<Id = CandidateId> {
    /// The resting place of the cursor above every candidate and the cutoff;
    /// focus is `None` here.
    Top,
    Cutoff,
    Row(Id),
}

struct App {
    model: Model,
    engine: Engine,
    focus: Option<CandidateId>,
    filter: String,
    editing_filter: bool,
    details: bool,
    help: bool,
    delete_now_armed: bool,
    next_batch: u64,
    message: Option<(Instant, String)>,
    notices: Vec<String>,
    /// Finished rows removed from view by compaction.
    compacted: HashSet<CandidateId>,
    /// First layout row shown in the viewport.
    offset: usize,
    /// Where the table was last drawn and what each visual row holds
    /// (`None` is the cutoff separator), for mapping mouse positions.
    table_area: Rect,
    table_rows: Vec<Slot>,
    dragging_cutoff: bool,
    page: usize,
    quitting: bool,
    /// Candidates the engine measures, in order; see `MEASURE_BELOW_CUTOFF`.
    wanted: Vec<CandidateId>,
    wanted_set: HashSet<CandidateId>,
    measure_paused: bool,
}

impl App {
    fn new(model: Model, engine: Engine) -> Self {
        App {
            model,
            engine,
            focus: None,
            filter: String::new(),
            editing_filter: false,
            details: false,
            help: false,
            delete_now_armed: false,
            next_batch: 1,
            message: None,
            notices: Vec::new(),
            compacted: HashSet::new(),
            offset: 0,
            table_area: Rect::default(),
            table_rows: Vec::new(),
            dragging_cutoff: false,
            page: 10,
            quitting: false,
            wanted: Vec::new(),
            wanted_set: HashSet::new(),
            measure_paused: false,
        }
    }

    fn sync_wanted(&mut self) {
        let selection = &self.model.selection;
        let end = (selection.cutoff() + MEASURE_BELOW_CUTOFF).min(selection.len());
        let wanted = &selection.order()[..end];
        if wanted != self.wanted.as_slice() {
            self.wanted = wanted.to_vec();
            self.wanted_set = wanted.iter().cloned().collect();
            self.engine.set_wanted(self.wanted.clone());
        }
    }

    /// Whether the candidate's size will be measured without further input.
    fn will_measure(&self, id: &CandidateId) -> bool {
        self.wanted_set.contains(id) || self.focus.as_ref() == Some(id)
    }

    fn toggle_measure_pause(&mut self) {
        self.measure_paused = !self.measure_paused;
        self.engine.set_measure_paused(self.measure_paused);
        self.say(if self.measure_paused {
            "size measurement paused (p to resume)"
        } else {
            "size measurement resumed"
        });
    }

    fn say(&mut self, message: impl Into<String>) {
        self.message = Some((Instant::now(), message.into()));
    }

    fn on_event(&mut self, event: Event) {
        let applied = self.model.apply(event);
        if let Some((id, outcome)) = applied.finished {
            if matches!(outcome, Outcome::Blocked { .. } | Outcome::Failed { .. }) {
                // Fresh evidence is required before a retry.
                self.engine.rescan_repo(id.repo.clone());
            }
            if !matches!(outcome, Outcome::Removed { .. } | Outcome::Cancelled) {
                self.say(format!("{}: {outcome}", self.path_of(&id)));
            }
        }
    }

    fn path_of(&self, id: &CandidateId) -> String {
        self.model.rows.get(id).map_or_else(
            || id.to_string(),
            |row| display_path(&row.candidate.seed.path),
        )
    }

    fn has_running(&self) -> bool {
        self.model
            .rows
            .values()
            .any(|row| row.delete.as_ref().is_some_and(DeleteState::is_active))
    }

    fn visible(&self) -> Vec<CandidateId> {
        let terms: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        self.model
            .selection
            .order()
            .iter()
            .filter(|id| !self.compacted.contains(*id))
            .filter(|id| {
                terms.is_empty()
                    || self.model.rows.get(*id).is_some_and(|row| {
                        let haystack = format!(
                            "{} {} {} {}",
                            row.candidate.seed.repo_label,
                            row.candidate.head_label(),
                            display_path(&row.candidate.seed.path),
                            state_text(row)
                        )
                        .to_lowercase();
                        terms.iter().all(|term| haystack.contains(term))
                    })
            })
            .cloned()
            .collect()
    }

    /// Returns true when the application should exit immediately.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        self.notices.clear();
        if self.editing_filter {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.editing_filter = false;
                }
                KeyCode::Enter => self.editing_filter = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            return false;
        }

        let armed = std::mem::take(&mut self.delete_now_armed);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return self.request_quit(true),
            KeyCode::Char('q') => return self.request_quit(false),
            KeyCode::Char('j') | KeyCode::Down => self.move_focus(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_focus(-1),
            KeyCode::Char('d') if ctrl => self.move_focus(self.page as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_focus(-(self.page as isize) / 2),
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
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Char('r') => {
                self.model.selection.rerank(&self.model.rows);
                self.say("reranked; review the selection before submitting");
            }
            KeyCode::Char('R') => {
                self.model.selection.reset(&self.model.rows);
                self.say("selection reset");
            }
            KeyCode::Enter | KeyCode::Tab => self.details = !self.details,
            KeyCode::Char('C') => self.compact(),
            KeyCode::Char('p') => self.toggle_measure_pause(),
            KeyCode::Char('?') => self.help = !self.help,
            _ => {}
        }
        false
    }

    fn request_quit(&mut self, force: bool) -> bool {
        if !self.has_running() {
            return true;
        }
        if force && self.quitting {
            return true;
        }
        self.quitting = true;
        self.engine.stop();
        self.say(
            "stopping: queued removals are cancelled, running ones finish (ctrl-c again to force)",
        );
        false
    }

    fn focus_index(&self, visible: &[CandidateId]) -> Option<usize> {
        let focus = self.focus.as_ref()?;
        visible.iter().position(|id| id == focus)
    }

    fn move_focus(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.focus = None;
            return;
        }
        // Cursor positions: 0 is the top slot, i + 1 is visible[i].
        let current = self.focus_index(&visible).map_or(0, |index| index + 1);
        let next = current.saturating_add_signed(delta).min(visible.len());
        if next == 0 {
            self.focus = None;
            return;
        }
        let id = visible[next - 1].clone();
        self.engine.prioritize(&id);
        self.focus = Some(id);
    }

    fn focused(&self) -> Option<CandidateId> {
        self.focused_in(&self.visible())
    }

    fn focused_in(&self, visible: &[CandidateId]) -> Option<CandidateId> {
        self.focus.clone().filter(|id| visible.contains(id))
    }

    fn toggle(&mut self) {
        let Some(id) = self.focused() else { return };
        match self.model.selection.toggle(&id, &self.model.rows) {
            Ok(_) => {}
            Err(ToggleError::BelowCutoff) => {
                self.say("below the cutoff: move down (j) or move the cutoff (J) to enable")
            }
            Err(ToggleError::NotEligible) => {
                let reason = self.model.rows[&id]
                    .eligibility
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                self.say(format!("not removable: {reason}"));
            }
            Err(ToggleError::Unavailable) => self.say("already queued or removed"),
        }
    }

    fn move_cutoff(&mut self, delta: isize) {
        if !self.filter.is_empty() {
            self.say("clear the filter (esc) to move the cutoff");
            return;
        }
        self.model.selection.move_cutoff(delta, &self.model.rows);
        self.focus_cutoff();
    }

    /// Puts the focus on the last row above the cutoff, so the cursor
    /// travels with the line.
    fn focus_cutoff(&mut self) {
        let selection = &self.model.selection;
        let Some(index) = selection.cutoff().checked_sub(1) else {
            self.focus = None;
            return;
        };
        if let Some(id) = selection.order().get(index).cloned() {
            self.engine.prioritize(&id);
            self.focus = Some(id);
        }
    }

    fn cutoff_to_focus(&mut self) {
        if !self.filter.is_empty() {
            self.say("clear the filter (esc) to move the cutoff");
            return;
        }
        // On the top slot this puts the cutoff above every candidate.
        let cutoff = self
            .focused()
            .and_then(|id| self.model.selection.position(&id))
            .map_or(0, |position| position + 1);
        self.model.selection.set_cutoff(cutoff, &self.model.rows);
    }

    fn submit(&mut self, ids: Vec<CandidateId>) -> usize {
        let requests: Vec<Request> = ids
            .iter()
            .filter_map(|id| self.model.rows.get(id))
            .filter(|row| row.eligibility.is_ok())
            .filter_map(|row| Request::from_candidate(&row.candidate))
            .collect();
        let batch = self.next_batch;
        self.next_batch += 1;
        let accepted = self.engine.submit(requests);
        for id in &accepted {
            self.model
                .set_delete_state(id, DeleteState::Queued { batch });
        }
        self.model.selection.refresh(&self.model.rows);
        accepted.len()
    }

    fn submit_selected(&mut self) {
        let selected = self.model.selection.selected(&self.model.rows);
        if selected.is_empty() {
            self.say("nothing selected: move the cutoff with J or =");
            return;
        }
        let visible: HashSet<CandidateId> = self.visible().into_iter().collect();
        let hidden = selected.iter().filter(|id| !visible.contains(*id)).count();
        let batch = self.next_batch;
        let queued = self.submit(selected);
        let hidden_note = if hidden > 0 {
            format!(" ({hidden} hidden by the filter)")
        } else {
            String::new()
        };
        self.say(format!("batch {batch}: {queued} queued{hidden_note}"));
    }

    fn arm_delete_now(&mut self) {
        let Some(id) = self.focused() else { return };
        let row = &self.model.rows[&id];
        if let Some(state) = row
            .delete
            .as_ref()
            .filter(|state| !state.allows_resubmission())
        {
            let text = delete_text(state);
            self.say(format!("already {text}"));
            return;
        }
        if let Err(block) = &row.eligibility {
            self.say(format!("not removable: {block}"));
            return;
        }
        self.delete_now_armed = true;
        self.say(format!(
            "press x again to delete {} now",
            display_path(&row.candidate.seed.path)
        ));
    }

    fn delete_now(&mut self) {
        let Some(id) = self.focused() else { return };
        let path = self.path_of(&id);
        if self.submit(vec![id]) == 1 {
            self.say(format!("deleting {path}"));
        } else {
            self.say(format!("{path} is already queued"));
        }
    }

    fn cancel_focused(&mut self) {
        let Some(id) = self.focused() else { return };
        match self.engine.cancel(&id) {
            CancelResult::Cancelled => {
                self.model
                    .set_delete_state(&id, DeleteState::Finished(Outcome::Cancelled));
                self.model.selection.refresh(&self.model.rows);
                self.say("cancelled");
            }
            CancelResult::Requested => self.say("cancelling before removal starts"),
            CancelResult::Running => self.say("removal already running; it cannot be cancelled"),
            CancelResult::NotQueued => self.say("nothing queued for this row"),
        }
    }

    fn compact(&mut self) {
        let finished: Vec<CandidateId> = self
            .model
            .rows
            .iter()
            .filter(|(_, row)| row.is_removed())
            .map(|(id, _)| id.clone())
            .collect();
        let count = finished.len();
        self.compacted.extend(finished);
        self.say(format!("hid {count} removed rows"));
    }

    /// Visual table row under a screen position, below the header.
    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let area = self.table_area;
        let inside = column >= area.x
            && column < area.x + area.width
            && row > area.y
            && row < area.y + area.height;
        if !inside {
            return None;
        }
        let index = usize::from(row - area.y - 1);
        (index < self.table_rows.len()).then_some(index)
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
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
                        if self.filter.is_empty() {
                            self.dragging_cutoff = true;
                        } else {
                            self.say("clear the filter (esc) to move the cutoff");
                        }
                    }
                    Slot::Row(id) => {
                        self.focus = Some(id.clone());
                        self.engine.prioritize(&id);
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

    /// Moves the cutoff so the separator follows the pointer: below the
    /// hovered row when dragging down, above it when dragging up.
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
            Slot::Row(id) => {
                let Some(position) = self.model.selection.position(id) else {
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
            self.model.selection.set_cutoff(cutoff, &self.model.rows);
        }
    }
}

mod view;
