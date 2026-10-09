//! Picker UI: one fuzzy list of worktrees, branches and requests across all
//! projects. Draws on stderr so stdout carries only the picked path.

use std::collections::{HashMap, HashSet};
use std::io::{self, Stderr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, select};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row as TableRow, Table};
use serde::Serialize;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use unicode_width::UnicodeWidthStr;

use git_yard::config::Config;
use git_yard::format::{self, truncate_end};
use git_yard::model::{Timestamp, now};
use git_yard::pick::engine::{Engine, Event};
use git_yard::pick::forge::{Forge, Request, Requests};
use git_yard::pick::launch::{self, Action, Handoff};
use git_yard::pick::rank::{self, Item, Ranker};
use git_yard::pick::store::{PickCache, pick_key};
use git_yard::pick::{Listing, Project, Target};
use git_yard::store::Paths;

const FRAME: Duration = Duration::from_millis(16);
const MESSAGE_TTL: Duration = Duration::from_secs(6);
/// Typing pauses this long before the shown projects are fetched.
const REFRESH_DELAY: Duration = Duration::from_millis(350);
/// Projects among this many top results are fetched for a query.
const REFRESH_TOP: usize = 8;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What the user chose, resolved after the terminal is restored.
struct Choice {
    project: Arc<Project>,
    target: Target,
    request: Option<(Forge, Request)>,
    key: String,
    action: Action,
    /// Print the path regardless of the requested handoff.
    print_only: bool,
}

/// What `--json` prints: the selection, without switching.
#[derive(Serialize)]
struct Selection<'a> {
    project: &'a str,
    common_dir: &'a Path,
    main: &'a Path,
    target: &'a Target,
    #[serde(skip_serializing_if = "Option::is_none")]
    forge: Option<Forge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a Request>,
    /// Name of the branch to create from the target (`ctrl-o`).
    #[serde(skip_serializing_if = "Option::is_none")]
    create: Option<&'a str>,
}

/// `handoff` of `None` prints the selection as JSON instead of switching.
pub fn run(
    config: Arc<Config>,
    paths: &Paths,
    query: String,
    handoff: Option<Handoff>,
) -> Result<ExitCode> {
    let mut cache = PickCache::load(&paths.pick_file);
    let (engine, events) = Engine::start(config.clone());
    let mut app = App::new(config.clone(), engine, &mut cache, query);

    let mut session = crate::terminal::stderr()?;
    let input = crate::terminal::input();
    let choice = app.run_loop(&mut session.terminal, &input.events, &events);
    drop(input);
    drop(session);
    let choice = choice?;

    cache.listings = app.listings.into_values().collect();
    cache.requests = app.requests.into_iter().collect();
    let now = now();
    let Some(choice) = choice else {
        save(&mut cache, paths, now);
        return Ok(ExitCode::from(130));
    };

    let Some(handoff) = handoff else {
        let selection = Selection {
            project: &choice.project.label,
            common_dir: &choice.project.common_dir,
            main: &choice.project.main_path,
            target: &choice.target,
            forge: choice.request.as_ref().map(|(forge, _)| *forge),
            request: choice.request.as_ref().map(|(_, request)| request),
            create: match &choice.action {
                Action::Open => None,
                Action::Create { name } => Some(name),
            },
        };
        println!("{}", serde_json::to_string(&selection)?);
        cache.picks.insert(choice.key, now);
        save(&mut cache, paths, now);
        return Ok(ExitCode::SUCCESS);
    };

    let switched = launch::switch(
        &config.pick,
        &choice.project,
        &choice.target,
        choice.request.as_ref(),
        &choice.action,
    );
    let switched = match switched {
        Ok(switched) => switched,
        Err(error) => {
            save(&mut cache, paths, now);
            eprintln!("git-yard: {error:#}");
            return Ok(ExitCode::FAILURE);
        }
    };
    cache.picks.insert(choice.key, now);
    let worktree = Target::Worktree {
        path: switched.path.clone(),
        branch: switched.branch.clone(),
    };
    cache
        .picks
        .insert(pick_key(&choice.project.common_dir, &worktree.key()), now);
    save(&mut cache, paths, now);

    let handoff = if choice.print_only {
        &Handoff::Print
    } else {
        &handoff
    };
    launch::hand_off(handoff, &choice.project, &switched)?;
    Ok(ExitCode::SUCCESS)
}

fn save(cache: &mut PickCache, paths: &Paths, now: Timestamp) {
    if let Err(error) = cache.save(&paths.pick_file, now) {
        eprintln!("git-yard: saving picker cache: {error:#}");
    }
}

type Term = Terminal<CrosstermBackend<Stderr>>;

struct App {
    config: Arc<Config>,
    engine: Engine,
    listings: HashMap<PathBuf, Listing>,
    requests: HashMap<PathBuf, Requests>,
    /// Projects confirmed by this run's discovery.
    fresh: HashSet<PathBuf>,
    picks: std::collections::BTreeMap<String, Timestamp>,
    items: Vec<Item>,
    ranker: Ranker,
    results: Vec<usize>,
    /// Index into `results`.
    cursor: usize,
    offset: usize,
    query: Input,
    scope: Option<PathBuf>,
    /// Name being typed for a new branch.
    prompt: Option<Input>,
    rerank: bool,
    /// Target to keep the cursor on across a re-rank. `results` is cleared
    /// whenever `items` changes, since it holds indices into it.
    anchor: Option<String>,
    refresh_at: Option<Instant>,
    fetching: HashSet<PathBuf>,
    querying: HashSet<PathBuf>,
    errors: Vec<String>,
    discovery_done: bool,
    projects: usize,
    message: Option<(Instant, String)>,
    now: Timestamp,
    list_area: Rect,
    last_click: Option<(Instant, usize)>,
}

impl App {
    fn new(config: Arc<Config>, engine: Engine, cache: &mut PickCache, query: String) -> Self {
        let listings: HashMap<PathBuf, Listing> = std::mem::take(&mut cache.listings)
            .into_iter()
            .map(|listing| (listing.project.common_dir.clone(), listing))
            .collect();
        let requests: HashMap<PathBuf, Requests> =
            std::mem::take(&mut cache.requests).into_iter().collect();
        let mut app = App {
            config,
            engine,
            listings,
            requests,
            fresh: HashSet::new(),
            picks: cache.picks.clone(),
            items: Vec::new(),
            ranker: Ranker::default(),
            results: Vec::new(),
            cursor: 0,
            offset: 0,
            refresh_at: (!query.is_empty()).then(Instant::now),
            query: Input::new(query),
            scope: None,
            prompt: None,
            rerank: true,
            anchor: None,
            fetching: HashSet::new(),
            querying: HashSet::new(),
            errors: Vec::new(),
            discovery_done: false,
            projects: 0,
            message: None,
            now: now(),
            list_area: Rect::default(),
            last_click: None,
        };
        let projects: Vec<PathBuf> = app.listings.keys().cloned().collect();
        for project in projects {
            app.rebuild(&project);
        }
        app
    }

    fn run_loop(
        &mut self,
        terminal: &mut Term,
        input: &Receiver<io::Result<TermEvent>>,
        events: &Receiver<Event>,
    ) -> Result<Option<Choice>> {
        let mut dirty = true;
        let mut last_draw = Instant::now() - FRAME;
        loop {
            if dirty && last_draw.elapsed() >= FRAME {
                self.settle();
                terminal.draw(|frame| self.draw(frame))?;
                last_draw = Instant::now();
                dirty = false;
            }
            if self
                .refresh_at
                .is_some_and(|at| at.elapsed() >= REFRESH_DELAY)
            {
                self.refresh_at = None;
                self.settle();
                self.refresh_shown();
                dirty = true;
            }
            let wait = if dirty {
                FRAME.saturating_sub(last_draw.elapsed())
            } else {
                self.refresh_at.map_or(Duration::from_secs(1), |at| {
                    REFRESH_DELAY.saturating_sub(at.elapsed())
                })
            };
            select! {
                recv(input) -> input => {
                    self.settle();
                    let event = input.context("terminal input disconnected")?
                        .context("reading terminal input")?;
                    match event {
                        TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                            if let Some(outcome) = self.on_key(key) {
                                return Ok(outcome);
                            }
                        }
                        TermEvent::Mouse(mouse) => {
                            if let Some(choice) = self.on_mouse(mouse) {
                                return Ok(Some(choice));
                            }
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                recv(events) -> event => {
                    let event = event.context("picker workers disconnected")?;
                    self.on_event(event);
                    for event in events.try_iter().take(5000) {
                        self.on_event(event);
                    }
                    dirty = true;
                }
                default(wait) => {
                    self.now = now();
                }
            }
        }
    }

    fn say(&mut self, message: impl Into<String>) {
        self.message = Some((Instant::now(), message.into()));
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Listed(listing) => {
                let common_dir = listing.project.common_dir.clone();
                self.fresh.insert(common_dir.clone());
                self.listings.insert(common_dir.clone(), listing);
                self.rebuild(&common_dir);
            }
            Event::ListFailed { common_dir, error } => {
                self.errors
                    .push(format!("{}: {error}", common_dir.display()));
            }
            Event::DiscoveryDone { projects } => {
                self.discovery_done = true;
                self.projects = projects;
                let vanished: Vec<PathBuf> = self
                    .listings
                    .keys()
                    .filter(|dir| !self.fresh.contains(*dir))
                    .cloned()
                    .collect();
                for dir in vanished {
                    self.listings.remove(&dir);
                    self.requests.remove(&dir);
                    self.rebuild(&dir);
                }
            }
            Event::Fetched { common_dir, error } => {
                self.fetching.remove(&common_dir);
                if let Some(error) = error {
                    let label = self.label(&common_dir);
                    self.errors.push(format!("fetch {label}: {error}"));
                }
            }
            Event::Requests { common_dir, result } => {
                self.querying.remove(&common_dir);
                match result {
                    Ok(requests) => {
                        self.requests.insert(common_dir.clone(), requests);
                        self.rebuild(&common_dir);
                    }
                    Err(error) => {
                        let label = self.label(&common_dir);
                        self.errors.push(format!("requests {label}: {error}"));
                    }
                }
            }
        }
    }

    fn label(&self, common_dir: &PathBuf) -> String {
        self.listings.get(common_dir).map_or_else(
            || common_dir.display().to_string(),
            |listing| listing.project.label.clone(),
        )
    }

    /// Replaces one project's rows.
    fn rebuild(&mut self, common_dir: &PathBuf) {
        if self.anchor.is_none() {
            self.anchor = self.selected().map(|item| item.key.clone());
        }
        self.results.clear();
        self.items
            .retain(|item| &item.project.common_dir != common_dir);
        if let Some(listing) = self.listings.get(common_dir) {
            self.items
                .extend(rank::items(listing, self.requests.get(common_dir)));
        }
        self.rerank = true;
    }

    /// Re-ranks after changes, keeping the cursor on the same target.
    fn settle(&mut self) {
        if !std::mem::take(&mut self.rerank) {
            return;
        }
        let selected = self.anchor.take();
        self.results = self.ranker.rank(
            &self.items,
            self.query.value(),
            self.scope.as_deref(),
            &self.picks,
            self.now,
        );
        self.cursor = selected
            .and_then(|key| {
                self.results
                    .iter()
                    .position(|&index| self.items[index].key == key)
            })
            .unwrap_or(self.cursor)
            .min(self.results.len().saturating_sub(1));
    }

    fn selected(&self) -> Option<&Item> {
        self.results
            .get(self.cursor)
            .map(|&index| &self.items[index])
    }

    /// Fetches and queries the projects the user is looking at.
    fn refresh_shown(&mut self) {
        let projects: Vec<Arc<Project>> = match &self.scope {
            Some(scope) => self
                .items
                .iter()
                .find(|item| &item.project.common_dir == scope)
                .map(|item| vec![item.project.clone()])
                .unwrap_or_default(),
            None if self.query.value().trim().is_empty() => Vec::new(),
            None => {
                let mut seen = HashSet::new();
                self.results
                    .iter()
                    .map(|&index| self.items[index].project.clone())
                    .filter(|project| seen.insert(project.common_dir.clone()))
                    .take(REFRESH_TOP)
                    .collect()
            }
        };
        let stale_after = self.config.pick.request_minutes as i64 * 60;
        for project in projects {
            if self.engine.fetch(&project) {
                self.fetching.insert(project.common_dir.clone());
            }
            let fresh = self
                .requests
                .get(&project.common_dir)
                .is_some_and(|requests| self.now - requests.fetched_at < stale_after);
            if !fresh && self.engine.query_requests(&project) {
                self.querying.insert(project.common_dir.clone());
            }
        }
    }

    fn query_changed(&mut self) {
        self.anchor = None;
        self.cursor = 0;
        self.offset = 0;
        self.rerank = true;
        self.refresh_at = Some(Instant::now());
    }

    fn set_scope(&mut self, scope: Option<PathBuf>) {
        self.scope = scope;
        self.query.reset();
        self.query_changed();
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.results.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    fn choose(&mut self, action: Action, print_only: bool) -> Option<Choice> {
        let item = self.selected()?;
        let choice = Choice {
            project: item.project.clone(),
            target: item.target.clone(),
            request: item.request.clone(),
            key: item.key.clone(),
            action,
            print_only,
        };
        // Catch impossible combinations before leaving the screen.
        match launch::validate(&choice.target, choice.request.as_ref(), &choice.action) {
            Ok(_) => Some(choice),
            Err(error) => {
                self.say(format!("{error:#}"));
                None
            }
        }
    }

    /// `Some(None)` quits, `Some(Some(choice))` opens a target.
    fn on_key(&mut self, key: KeyEvent) -> Option<Option<Choice>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return Some(None);
        }

        if let Some(mut name) = self.prompt.take() {
            match key.code {
                KeyCode::Esc => return None,
                KeyCode::Enter => {
                    let branch = name.value().trim().to_owned();
                    if branch.is_empty() {
                        self.say("enter a branch name");
                    } else if let Some(choice) = self.choose(Action::Create { name: branch }, false)
                    {
                        return Some(Some(choice));
                    }
                    self.prompt = Some(name);
                    return None;
                }
                KeyCode::Char(c) if c.is_whitespace() => {}
                _ => {
                    name.handle_event(&TermEvent::Key(key));
                }
            }
            self.prompt = Some(name);
            return None;
        }

        match key.code {
            KeyCode::Enter => return self.choose(Action::Open, alt).map(Some),
            KeyCode::Esc => {
                if !self.query.value().is_empty() {
                    self.query.reset();
                    self.query_changed();
                } else if self.scope.is_some() {
                    self.set_scope(None);
                } else {
                    return Some(None);
                }
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('p' | 'k') if ctrl => self.move_cursor(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.page() as isize)),
            KeyCode::PageDown => self.move_cursor(self.page() as isize),
            KeyCode::Tab => {
                if self.scope.is_some() {
                    self.set_scope(None);
                } else if let Some(item) = self.selected() {
                    let scope = item.project.common_dir.clone();
                    self.set_scope(Some(scope));
                }
            }
            KeyCode::BackTab => self.set_scope(None),
            KeyCode::Char('o') if ctrl => match self.selected() {
                Some(item) if matches!(item.target, Target::Request { .. }) => {
                    self.say("open the request first, then branch off its worktree");
                }
                Some(_) => self.prompt = Some(Input::default()),
                None => {}
            },
            KeyCode::Backspace if self.query.value().is_empty() && self.scope.is_some() => {
                self.set_scope(None);
            }
            _ => {
                if self
                    .query
                    .handle_event(&TermEvent::Key(key))
                    .is_some_and(|change| change.value)
                {
                    self.query_changed();
                }
            }
        }
        None
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Choice> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(3),
            MouseEventKind::ScrollUp => self.move_cursor(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if mouse.row < area.y || mouse.row >= area.y + area.height {
                    return None;
                }
                let index = self.offset + usize::from(mouse.row - area.y);
                if index >= self.results.len() {
                    return None;
                }
                self.cursor = index;
                let double = self
                    .last_click
                    .is_some_and(|(at, last)| last == index && at.elapsed() < DOUBLE_CLICK);
                self.last_click = Some((Instant::now(), index));
                if double {
                    return self.choose(Action::Open, false);
                }
            }
            _ => {}
        }
        None
    }

    fn page(&self) -> usize {
        usize::from(self.list_area.height.max(1))
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, input, list, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.list_area = list;

        frame.render_widget(Paragraph::new(self.header_line()), header);
        self.draw_input(frame, input);
        self.draw_list(frame, list);
        frame.render_widget(Paragraph::new(self.footer_line()), footer);
    }

    fn header_line(&self) -> Line<'static> {
        let mut spans = vec![Span::from("git-yard pick").bold(), Span::from("  ")];
        if self.discovery_done {
            spans.push(Span::from(format!("{} projects", self.projects)));
        } else {
            spans.push(Span::from(format!("discovering… {}", self.fresh.len())).cyan());
        }
        spans.push(Span::from(format!(" · {} shown", self.results.len())).dim());
        if !self.fetching.is_empty() {
            spans.push(Span::from(format!(" · fetching {}", self.fetching.len())).cyan());
        }
        if !self.querying.is_empty() {
            spans.push(Span::from(format!(" · requests {}", self.querying.len())).cyan());
        }
        if !self.errors.is_empty() {
            spans.push(Span::from(format!(" · {} errors", self.errors.len())).red());
        }
        Line::from(spans)
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        if let Some(name) = &self.prompt {
            let base = self.selected().map(Item::branch_text).unwrap_or_default();
            let prefix = Line::from(format!("new branch from {base}: ")).yellow();
            crate::text_input::draw(frame, area, prefix, name);
        } else {
            let mut spans = Vec::new();
            if let Some(scope) = &self.scope {
                spans.push(Span::from(format!("[{}] ", self.label(scope))).cyan());
            }
            spans.push(Span::from("> ").bold());
            crate::text_input::draw(frame, area, Line::from(spans), &self.query);
        }
    }

    fn footer_line(&self) -> Line<'static> {
        if let Some((at, message)) = &self.message
            && at.elapsed() < MESSAGE_TTL
        {
            return Line::from(Span::from(message.clone()).yellow());
        }
        if self.prompt.is_some() {
            return Line::from("enter create worktree · esc cancel").dim();
        }
        let first_error = self
            .errors
            .last()
            .map(|error| format!(" · last error: {error}"));
        Line::from(vec![
            Span::from(
                "enter open · alt-enter print path · tab project · ctrl-o new branch · esc back",
            )
            .dim(),
            Span::from(first_error.unwrap_or_default()).red().dim(),
        ])
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let height = usize::from(area.height);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
        let shown = &self.results[self.offset.min(self.results.len())..];
        let shown = &shown[..shown.len().min(height)];

        // Column widths follow the longest content among the first rows of
        // the ranking, so they stay put while scrolling.
        let sample = self
            .results
            .iter()
            .take(500)
            .map(|&index| &self.items[index]);
        let (mut project_width, mut branch_width) = (0, 0);
        for item in sample {
            project_width = project_width.max(item.project.label.width());
            branch_width = branch_width.max(item.branch_text().width());
        }
        let available = usize::from(area.width).saturating_sub(2 + 5 + 3);
        let project_width = project_width.clamp(8, 30).min(available * 3 / 10);
        let branch_width = branch_width.clamp(8, 45).min(available * 35 / 100);
        let info_width = available.saturating_sub(project_width + branch_width);

        let rows = shown.iter().enumerate().map(|(row, &index)| {
            let item = &self.items[index];
            let selected = self.offset + row == self.cursor;
            let (marker, color) = match item.target {
                Target::Worktree { .. } => ("●", Color::Green),
                Target::Local { .. } => ("○", Color::Reset),
                Target::Remote { .. } => ("◌", Color::Blue),
                Target::Request { .. } => ("◇", Color::Magenta),
            };
            let mut branch = Cell::from(truncate_end(&item.branch_text(), branch_width));
            if item.base {
                branch = branch.bold();
            }
            let age = item
                .time
                .map_or(String::new(), |time| format::age(self.now - time));
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            TableRow::new(vec![
                Cell::from(marker).fg(color),
                Cell::from(truncate_end(&item.project.label, project_width)),
                branch,
                Cell::from(truncate_end(&item.info_text(), info_width)).dim(),
                Cell::from(age).dim(),
            ])
            .style(style)
        });
        let widths = [
            Constraint::Length(1),
            Constraint::Length(project_width as u16),
            Constraint::Length(branch_width as u16),
            Constraint::Length(info_width as u16),
            Constraint::Length(4),
        ];
        frame.render_widget(Table::new(rows, widths).column_spacing(1), area);
    }
}
