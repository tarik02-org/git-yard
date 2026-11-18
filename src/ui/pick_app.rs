//! Picker state: one fuzzy list of worktrees, branches and requests across
//! all projects. Keys act on the rows as last drawn, so a list reshuffled
//! by background updates never opens a row the user did not see.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::core::config::Config;
use crate::core::ids::{RepoKey, Timestamp, now};
use crate::git::remote::Forge;
use crate::pick::engine::{Control, Event};
use crate::pick::forge::{Request, Requests};
use crate::pick::launch::{self, Action};
use crate::pick::listing::{Listing, Project, Target};
use crate::pick::rank::{self, Item, Ranker};

use super::event_loop::{App, Flow};
use super::input::{DOUBLE_CLICK, Toast};
use super::view;

/// Typing pauses this long before the shown projects are fetched.
const REFRESH_DELAY: Duration = Duration::from_millis(350);
/// Projects among this many top results are fetched for a query.
const REFRESH_TOP: usize = 8;

/// What the user chose, resolved after the terminal is restored.
#[derive(Debug)]
pub struct Choice {
    pub project: Arc<Project>,
    pub target: Target,
    pub request: Option<(Forge, Request)>,
    /// Pick history key.
    pub key: String,
    pub action: Action,
    /// Print the path regardless of the requested handoff (Alt-Enter).
    pub print_only: bool,
}

pub struct PickApp<C: Control> {
    pub config: Arc<Config>,
    pub control: C,
    pub listings: HashMap<RepoKey, Listing>,
    pub requests: HashMap<RepoKey, Requests>,
    /// Projects confirmed by this run's discovery.
    pub fresh: HashSet<RepoKey>,
    pub picks: BTreeMap<String, Timestamp>,
    pub items: Vec<Item>,
    ranker: Ranker,
    pub results: Vec<usize>,
    /// Index into `results`.
    pub cursor: usize,
    pub offset: usize,
    pub query: Input,
    pub scope: Option<RepoKey>,
    /// Name being typed for a new branch.
    pub prompt: Option<Input>,
    rerank: bool,
    refresh_at: Option<Instant>,
    pub fetching: HashSet<RepoKey>,
    pub querying: HashSet<RepoKey>,
    pub errors: Vec<String>,
    pub discovery_done: bool,
    pub projects: usize,
    pub toast: Toast,
    pub now: Timestamp,
    pub list_area: Rect,
    /// Keys of the rows in the last frame, top to bottom, for the mouse.
    pub drawn: Vec<String>,
    /// The row the user put the cursor on. Reranks keep the cursor on it;
    /// if it disappears, Enter refuses instead of opening a neighbour.
    cursor_key: Option<String>,
    last_click: Option<(Instant, String)>,
}

impl<C: Control> PickApp<C> {
    pub fn new(
        config: Arc<Config>,
        control: C,
        listings: Vec<Listing>,
        requests: Vec<(RepoKey, Requests)>,
        picks: BTreeMap<String, Timestamp>,
        query: String,
    ) -> Self {
        let mut app = PickApp {
            config,
            control,
            listings: listings
                .into_iter()
                .map(|listing| (listing.project.repo.clone(), listing))
                .collect(),
            requests: requests.into_iter().collect(),
            fresh: HashSet::new(),
            picks,
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
            fetching: HashSet::new(),
            querying: HashSet::new(),
            errors: Vec::new(),
            discovery_done: false,
            projects: 0,
            toast: Toast::default(),
            now: now(),
            list_area: Rect::default(),
            drawn: Vec::new(),
            cursor_key: None,
            last_click: None,
        };
        let repos: Vec<RepoKey> = app.listings.keys().cloned().collect();
        for repo in repos {
            app.rebuild(&repo);
        }
        app
    }

    pub fn label(&self, repo: &RepoKey) -> String {
        self.listings
            .get(repo)
            .map_or_else(|| repo.to_string(), |listing| listing.project.label.clone())
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Listed(listing) => {
                let repo = listing.project.repo.clone();
                self.fresh.insert(repo.clone());
                self.listings.insert(repo.clone(), listing);
                self.rebuild(&repo);
            }
            Event::ListFailed { repo, error } => {
                self.errors.push(format!("{repo}: {error}"));
            }
            Event::DiscoveryDone { projects } => {
                self.discovery_done = true;
                self.projects = projects;
                let vanished: Vec<RepoKey> = self
                    .listings
                    .keys()
                    .filter(|repo| !self.fresh.contains(*repo))
                    .cloned()
                    .collect();
                for repo in vanished {
                    self.listings.remove(&repo);
                    self.requests.remove(&repo);
                    self.rebuild(&repo);
                }
            }
            Event::Fetched { repo, error } => {
                self.fetching.remove(&repo);
                if let Some(error) = error {
                    let label = self.label(&repo);
                    self.errors.push(format!("fetch {label}: {error}"));
                }
            }
            Event::Requests { repo, result } => {
                self.querying.remove(&repo);
                match result {
                    Ok(requests) => {
                        self.requests.insert(repo.clone(), requests);
                        self.rebuild(&repo);
                    }
                    Err(error) => {
                        let label = self.label(&repo);
                        self.errors.push(format!("requests {label}: {error}"));
                    }
                }
            }
        }
    }

    /// Replaces one project's rows.
    fn rebuild(&mut self, repo: &RepoKey) {
        self.results.clear();
        self.items.retain(|item| &item.project.repo != repo);
        if let Some(listing) = self.listings.get(repo) {
            self.items
                .extend(rank::items(listing, self.requests.get(repo)));
        }
        self.rerank = true;
    }

    /// Re-ranks after changes, keeping the cursor on the same target.
    pub fn refresh(&mut self) {
        if !std::mem::take(&mut self.rerank) {
            return;
        }
        self.results = self.ranker.rank(
            &self.items,
            self.query.value(),
            self.scope.as_ref().map(RepoKey::path),
            &self.picks,
            self.now,
        );
        let kept = self.cursor_key.as_ref().and_then(|key| self.position(key));
        self.cursor = kept
            .unwrap_or(self.cursor)
            .min(self.results.len().saturating_sub(1));
        if self.cursor_key.is_none() {
            self.cursor_key = self.selected().map(|item| item.key.clone());
        }
    }

    pub fn selected(&self) -> Option<&Item> {
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
                .find(|item| &item.project.repo == scope)
                .map(|item| vec![item.project.clone()])
                .unwrap_or_default(),
            None if self.query.value().trim().is_empty() => Vec::new(),
            None => {
                let mut seen = HashSet::new();
                let shown: Vec<Arc<Project>> = self
                    .results
                    .iter()
                    .map(|&index| self.items[index].project.clone())
                    .filter(|project| seen.insert(project.repo.clone()))
                    .take(REFRESH_TOP)
                    .collect();
                if shown.is_empty() {
                    // A request number matches nothing until requests are
                    // loaded: refresh projects named by any search word.
                    self.projects_named_in_query()
                } else {
                    shown
                }
            }
        };
        let stale_after = self.config.pick.request_minutes as i64 * 60;
        for project in projects {
            if self.control.fetch(&project) {
                self.fetching.insert(project.repo.clone());
            }
            let fresh = self
                .requests
                .get(&project.repo)
                .is_some_and(|requests| self.now - requests.fetched_at < stale_after);
            if !fresh && self.control.query_requests(&project) {
                self.querying.insert(project.repo.clone());
            }
        }
    }

    fn projects_named_in_query(&self) -> Vec<Arc<Project>> {
        let words: Vec<String> = self
            .query
            .value()
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut seen = HashSet::new();
        self.items
            .iter()
            .map(|item| &item.project)
            .filter(|project| {
                let label = project.label.to_lowercase();
                words.iter().any(|word| label.contains(word.as_str()))
            })
            .filter(|project| seen.insert(project.repo.clone()))
            .take(REFRESH_TOP)
            .cloned()
            .collect()
    }

    fn query_changed(&mut self) {
        self.cursor_key = None;
        self.cursor = 0;
        self.offset = 0;
        self.rerank = true;
        self.refresh_at = Some(Instant::now());
    }

    fn set_scope(&mut self, scope: Option<RepoKey>) {
        self.scope = scope;
        self.query.reset();
        self.query_changed();
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.results.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
        self.cursor_key = self.selected().map(|item| item.key.clone());
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.results
            .iter()
            .position(|&index| self.items[index].key == key)
    }

    /// Chooses the row the user put the cursor on.
    fn choose(&mut self, key: Option<String>, action: Action, print_only: bool) -> Option<Choice> {
        let key = key?;
        let Some(item) = self
            .position(&key)
            .map(|position| &self.items[self.results[position]])
        else {
            self.toast.say("that row is gone; pick again");
            self.cursor_key = self.selected().map(|item| item.key.clone());
            return None;
        };
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
            Ok(()) => Some(choice),
            Err(error) => {
                self.toast.say(format!("{error:#}"));
                None
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Flow<Option<Choice>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return Flow::Exit(None);
        }

        if let Some(mut name) = self.prompt.take() {
            match key.code {
                KeyCode::Esc => return Flow::Continue,
                KeyCode::Enter => {
                    let branch = name.value().trim().to_owned();
                    if branch.is_empty() {
                        self.toast.say("enter a branch name");
                    } else if let Some(choice) = self.choose(
                        self.cursor_key.clone(),
                        Action::Create { name: branch },
                        false,
                    ) {
                        return Flow::Exit(Some(choice));
                    }
                }
                KeyCode::Char(c) if c.is_whitespace() => {}
                _ => {
                    name.handle_event(&TermEvent::Key(key));
                }
            }
            self.prompt = Some(name);
            return Flow::Continue;
        }

        let page = usize::from(self.list_area.height.max(1)) as isize;
        match key.code {
            KeyCode::Enter => {
                if let Some(choice) = self.choose(self.cursor_key.clone(), Action::Open, alt) {
                    return Flow::Exit(Some(choice));
                }
            }
            KeyCode::Esc => {
                if !self.query.value().is_empty() {
                    self.query.reset();
                    self.query_changed();
                } else if self.scope.is_some() {
                    self.set_scope(None);
                } else {
                    return Flow::Exit(None);
                }
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('p' | 'k') if ctrl => self.move_cursor(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Tab => {
                if self.scope.is_some() {
                    self.set_scope(None);
                } else if let Some(item) = self.selected() {
                    let scope = item.project.repo.clone();
                    self.set_scope(Some(scope));
                }
            }
            KeyCode::BackTab => self.set_scope(None),
            KeyCode::Char('o') if ctrl => match self.selected() {
                Some(item) if matches!(item.target, Target::Request { .. }) => {
                    self.toast
                        .say("open the request first, then branch off its worktree");
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
        Flow::Continue
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Flow<Option<Choice>> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(3),
            MouseEventKind::ScrollUp => self.move_cursor(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if mouse.row < area.y || mouse.row >= area.y + area.height {
                    return Flow::Continue;
                }
                let Some(key) = self.drawn.get(usize::from(mouse.row - area.y)).cloned() else {
                    return Flow::Continue;
                };
                if let Some(position) = self.position(&key) {
                    self.cursor = position;
                    self.cursor_key = Some(key.clone());
                }
                let double = self
                    .last_click
                    .as_ref()
                    .is_some_and(|(at, last)| *last == key && at.elapsed() < DOUBLE_CLICK);
                self.last_click = Some((Instant::now(), key.clone()));
                if double && let Some(choice) = self.choose(Some(key), Action::Open, false) {
                    return Flow::Exit(Some(choice));
                }
            }
            _ => {}
        }
        Flow::Continue
    }
}

impl<C: Control> App for PickApp<C> {
    type Event = Event;
    type Output = Option<Choice>;

    fn on_input(&mut self, event: TermEvent) -> Flow<Option<Choice>> {
        match event {
            TermEvent::Key(key) => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            _ => Flow::Continue,
        }
    }

    fn on_event(&mut self, event: Event) {
        self.apply(event);
    }

    fn on_tick(&mut self) {
        self.now = now();
    }

    fn on_signal(&mut self, _repeated: bool) -> Flow<Option<Choice>> {
        Flow::Exit(None)
    }

    fn settle(&mut self) {
        self.refresh();
    }

    fn wake_at(&self) -> Option<Instant> {
        self.refresh_at.map(|at| at + REFRESH_DELAY)
    }

    fn on_wake(&mut self) {
        self.refresh_at = None;
        self.refresh_shown();
    }

    fn draw(&mut self, frame: &mut Frame) {
        view::pick::draw(self, frame);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use crate::core::ids::YardPath;
    use crate::pick::listing::Entry;

    struct Quiet;

    impl Control for Quiet {
        fn fetch(&self, _: &Project) -> bool {
            false
        }
        fn query_requests(&self, _: &Project) -> bool {
            false
        }
    }

    fn listing(label: &str, branches: &[(&str, i64)]) -> Listing {
        Listing {
            project: Project {
                repo: RepoKey::new(format!("/{label}/.git")),
                label: label.into(),
                main_path: YardPath::new(format!("/{label}")),
                remotes: vec![],
            },
            entries: branches
                .iter()
                .map(|(branch, time)| Entry {
                    target: Target::Worktree {
                        path: YardPath::new(format!("/{label}.{branch}")),
                        branch: Some((*branch).into()),
                    },
                    time: Some(*time),
                    subject: None,
                    base: false,
                })
                .collect(),
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn enter_opens_the_row_the_cursor_was_put_on_even_if_the_list_moved() {
        let mut app = PickApp::new(
            Arc::new(Config::default()),
            Quiet,
            vec![listing("a", &[("old", 10)])],
            vec![],
            BTreeMap::new(),
            String::new(),
        );
        app.refresh();
        assert_eq!(app.selected().unwrap().project.label, "a");

        // A newer worktree arrives before the key is handled and ranks on
        // top, where the cursor index pointed.
        app.apply(Event::Listed(listing("b", &[("new", 99)])));
        app.refresh();
        let Flow::Exit(Some(choice)) = app.on_key(enter()) else {
            panic!("enter did not choose");
        };
        assert_eq!(choice.project.label, "a");

        // Typing ahead acts on the moved cursor without waiting for a frame.
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        let Flow::Exit(Some(choice)) = app.on_key(enter()) else {
            panic!("enter did not choose");
        };
        assert_eq!(choice.project.label, "b");

        // A row that vanished is not replaced by its neighbour.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.listings.remove(&RepoKey::new("/a/.git"));
        app.rebuild(&RepoKey::new("/a/.git"));
        app.refresh();
        assert!(matches!(app.on_key(enter()), Flow::Continue));
        let Flow::Exit(Some(choice)) = app.on_key(enter()) else {
            panic!("second enter did not choose");
        };
        assert_eq!(choice.project.label, "b");
    }
}
