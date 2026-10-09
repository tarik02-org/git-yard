use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row as TableRow, Table, TableState, Wrap};
use unicode_width::UnicodeWidthStr;

use git_yard::discovery::display_path;
use git_yard::engine::DeleteState;
use git_yard::format::{self, truncate_end, truncate_middle};
use git_yard::model::{CandidateId, DirState, Integration, Obs};
use git_yard::remove::Outcome;
use git_yard::state::Row;

use super::{App, MESSAGE_TTL, Pin, Slot};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Mark,
    Repo,
    Branch,
    Commit,
    Path,
    Rate,
    Used,
    Age,
    Size,
    State,
}

/// Columns in display order.
const COLUMNS: [Col; 10] = [
    Col::Mark,
    Col::Repo,
    Col::Branch,
    Col::Commit,
    Col::Path,
    Col::Rate,
    Col::Used,
    Col::Age,
    Col::Size,
    Col::State,
];

/// Columns hidden first when the terminal is too narrow for every minimum.
const DROP_ORDER: [Col; 4] = [Col::Path, Col::Age, Col::Commit, Col::Used];

impl Col {
    fn title(self) -> &'static str {
        match self {
            Col::Mark => "",
            Col::Repo => "repo",
            Col::Branch => "branch",
            Col::Commit => "head",
            Col::Path => "path",
            Col::Rate => "rate",
            Col::Used => "used",
            Col::Age => "age",
            Col::Size => "size",
            Col::State => "state",
        }
    }

    fn min_width(self) -> usize {
        match self {
            Col::Mark => 3,
            Col::Repo => 10,
            Col::Branch => 12,
            Col::Commit => 24,
            Col::Path => 20,
            Col::Rate => 4,
            Col::Used => 4,
            Col::Age => 4,
            Col::Size => 6,
            Col::State => 10,
        }
    }

    /// Upper bound before content is known; `None` for fixed-width columns.
    fn max_width(self) -> Option<usize> {
        match self {
            Col::Repo => Some(32),
            Col::Branch => Some(36),
            Col::Commit => Some(usize::MAX),
            Col::Path => Some(usize::MAX),
            Col::State => Some(24),
            _ => None,
        }
    }

    /// Share of spare width; the commit subject is the most useful filler.
    fn weight(self) -> usize {
        match self {
            Col::Commit => 3,
            _ => 1,
        }
    }
}

/// Picks the columns that fit `width` and sizes them. Every column gets its
/// minimum; spare width is shared by weight among growable columns, each
/// capped at its longest visible content so short labels waste nothing.
fn columns_for(width: u16, content: impl Fn(Col) -> usize) -> (Vec<Col>, Vec<u16>) {
    let width = usize::from(width);
    let mut columns = COLUMNS.to_vec();
    for dropped in DROP_ORDER {
        let needed: usize = columns.iter().map(|column| column.min_width() + 1).sum();
        if needed <= width {
            break;
        }
        columns.retain(|column| *column != dropped);
    }

    let caps: Vec<usize> = columns
        .iter()
        .map(|column| match column.max_width() {
            Some(max) => content(*column)
                .max(column.title().len())
                .clamp(column.min_width(), max),
            None => column.min_width(),
        })
        .collect();
    let mut sizes: Vec<usize> = columns.iter().map(|column| column.min_width()).collect();
    let spacing = columns.len().saturating_sub(1);
    let mut spare = width.saturating_sub(sizes.iter().sum::<usize>() + spacing);
    // The path is the least useful filler: it only grows once everything
    // else is shown in full.
    for phase in [false, true] {
        loop {
            let growing: Vec<usize> = (0..columns.len())
                .filter(|&i| (columns[i] == Col::Path) == phase && sizes[i] < caps[i])
                .collect();
            let total_weight: usize = growing.iter().map(|&i| columns[i].weight()).sum();
            if spare == 0 || total_weight == 0 {
                break;
            }
            let round = spare;
            for &i in &growing {
                let share = (round * columns[i].weight() / total_weight).max(1);
                let grow = share.min(caps[i] - sizes[i]).min(spare);
                sizes[i] += grow;
                spare -= grow;
            }
        }
    }
    let sizes = sizes
        .into_iter()
        .map(|size| u16::try_from(size).unwrap_or(u16::MAX))
        .collect();
    (columns, sizes)
}

/// Untruncated text width of a growable column for one row.
fn content_width(row: &Row, column: Col) -> usize {
    let candidate = &row.candidate;
    match column {
        Col::Repo => candidate.seed.repo_label.width(),
        Col::Branch => candidate.head_label().width(),
        Col::Commit => {
            let subject = candidate
                .git
                .value()
                .and_then(|git| git.subject.as_deref())
                .unwrap_or("");
            8 + subject.width()
        }
        Col::Path => display_path(&candidate.seed.path).width(),
        Col::State => state_text(row).width(),
        _ => 0,
    }
}

/// Rows kept between the cursor and the top or bottom edge of the list.
const SCROLL_PADDING: usize = 3;

impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        let notice_height = self.notices.len().min(3) as u16;
        let [header, notices, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(notice_height),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(frame.area());

        frame.render_widget(Paragraph::new(self.header_line()), header);
        if notice_height > 0 {
            let lines: Vec<Line> = self
                .notices
                .iter()
                .map(|notice| Line::from(format!("! {notice}")).yellow())
                .collect();
            frame.render_widget(Paragraph::new(lines), notices);
        }

        let visible = self.visible();
        let focus = self.focused_in(&visible);
        let (list_area, details_area) = if self.details {
            let [list, details] =
                Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                    .areas(body);
            (list, Some(details))
        } else {
            (body, None)
        };
        self.page = list_area.height.saturating_sub(2).max(1) as usize;
        self.draw_table(frame, list_area, &visible, focus.as_ref());
        if let Some(area) = details_area {
            self.draw_details(frame, area, focus.as_ref());
        }
        self.draw_footer(frame, footer, &visible);
    }

    fn header_line(&self) -> Line<'static> {
        let model = &self.model;
        let total = model.rows.len();
        let mut spans = vec![Span::from("git-yard").bold(), Span::from("  ")];
        if !model.discovery_done {
            spans.push(Span::from(format!("discovering… {total} found")).cyan());
        } else {
            spans.push(Span::from(format!(
                "{} repos · {total} candidates",
                model.repos
            )));
        }
        let assessing = total - model.assessed();
        let (measuring, unmeasured) = model
            .rows
            .iter()
            .filter(|(_, row)| !row.candidate.usage.is_settled())
            .fold((0, 0), |(measuring, unmeasured), (id, _)| {
                if self.will_measure(id) {
                    (measuring + 1, unmeasured)
                } else {
                    (measuring, unmeasured + 1)
                }
            });
        let deleting = model
            .rows
            .values()
            .filter(|row| row.delete.as_ref().is_some_and(DeleteState::is_active))
            .count();
        let held = if self.measure_paused {
            " (paused)"
        } else if deleting > 0 {
            " (held while deleting)"
        } else {
            ""
        };
        if assessing > 0 {
            let held = if deleting > 0 {
                " (held while deleting)"
            } else {
                ""
            };
            spans.push(Span::from(format!(" · assessing {assessing}{held}")).cyan());
        }
        if measuring > 0 {
            spans.push(Span::from(format!(" · measuring {measuring}{held}")).cyan());
        } else if self.measure_paused {
            spans.push(Span::from(" · measuring paused").cyan());
        }
        if unmeasured > 0 {
            spans.push(Span::from(format!(" · {unmeasured} unmeasured")).dim());
        }
        if deleting > 0 {
            spans.push(Span::from(format!(" · deleting {deleting}")).magenta());
        }
        if !model.errors.is_empty() {
            spans.push(Span::from(format!(" · {} repo errors", model.errors.len())).red());
        }
        if self.quitting {
            spans.push(Span::from(" · stopping").yellow());
        }
        Line::from(spans)
    }

    fn draw_table(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        visible: &[CandidateId],
        focus: Option<&CandidateId>,
    ) {
        let cutoff = self.model.selection.cutoff();

        // Layout of every visual row; cheap, no text is built here.
        let mut layout: Vec<Slot<&CandidateId>> = Vec::with_capacity(visible.len() + 2);
        layout.push(Slot::Top);
        let mut separator_drawn = false;
        for id in visible {
            let above = self
                .model
                .selection
                .position(id)
                .is_some_and(|p| p < cutoff);
            if !above && !separator_drawn {
                layout.push(Slot::Cutoff);
                separator_drawn = true;
            }
            layout.push(Slot::Row(id));
        }
        if !separator_drawn {
            layout.push(Slot::Cutoff);
        }

        // Only rows inside the viewport are rendered.
        let height = usize::from(area.height.saturating_sub(1)).max(1);
        let focus_row = match focus {
            None => 0,
            Some(focus) => layout
                .iter()
                .position(|slot| *slot == Slot::Row(focus))
                .unwrap_or(0),
        };
        // Keep a margin around the cursor so the view scrolls before it
        // reaches an edge.
        let padding = SCROLL_PADDING.min(height.saturating_sub(1) / 2);
        if focus_row < self.offset + padding {
            self.offset = focus_row.saturating_sub(padding);
        } else if focus_row + padding >= self.offset + height {
            self.offset = focus_row + padding + 1 - height;
        }
        self.offset = self.offset.min(layout.len().saturating_sub(height));
        let end = (self.offset + height).min(layout.len());
        let mut window: Vec<(Slot<&CandidateId>, Pin)> = layout[self.offset..end]
            .iter()
            .map(|slot| (*slot, Pin::None))
            .collect();

        // The cutoff line sticks to the edge it scrolled past, replacing the
        // edge row; padding keeps the cursor off that row.
        let cutoff_row = layout.iter().position(|slot| *slot == Slot::Cutoff);
        if let Some(cutoff_row) = cutoff_row
            && window.len() > 1
        {
            if cutoff_row < self.offset {
                window[0] = (Slot::Cutoff, Pin::Above);
            } else if cutoff_row >= end && end - self.offset == height {
                let last = window.len() - 1;
                window[last] = (Slot::Cutoff, Pin::Below);
            }
        }
        let focus_row = window.iter().position(|(slot, _)| match (slot, focus) {
            (Slot::Top, None) => true,
            (Slot::Row(id), Some(focus)) => *id == focus,
            _ => false,
        });

        let (columns, widths) = columns_for(area.width, |column| {
            // All rows, not just the viewport, so widths stay put while scrolling.
            visible
                .iter()
                .filter_map(|id| self.model.rows.get(id))
                .map(|row| content_width(row, column))
                .max()
                .unwrap_or(0)
        });
        let rows: Vec<TableRow> = window
            .iter()
            .map(|(slot, pin)| match slot {
                Slot::Row(id) => self.table_row(id, cutoff, &columns, &widths),
                Slot::Top => {
                    let cells = columns
                        .iter()
                        .map(|column| Cell::from(if *column == Col::Repo { "(top)" } else { "" }));
                    TableRow::new(cells).style(Style::new().fg(Color::DarkGray))
                }
                Slot::Cutoff => {
                    let label = match pin {
                        Pin::None => "cutoff ",
                        Pin::Above => "cutoff ▲ ",
                        Pin::Below => "cutoff ▼ ",
                    };
                    // Every cell is filled with rule characters, truncated to
                    // its column, so the line spans the table at any width;
                    // the label sits in the repo column, which is always shown.
                    let rule = "─".repeat(usize::from(area.width));
                    let cells = columns.iter().map(|column| {
                        Cell::from(if *column == Col::Repo {
                            format!("{label}{rule}")
                        } else {
                            rule.clone()
                        })
                    });
                    TableRow::new(cells).style(Style::new().fg(Color::Yellow))
                }
            })
            .collect();
        let header =
            TableRow::new(columns.iter().map(|column| column.title())).style(Style::new().bold());
        let table = Table::new(rows, widths.iter().map(|width| Constraint::Length(*width)))
            .header(header)
            .column_spacing(1)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(focus_row);
        frame.render_stateful_widget(table, area, &mut state);
        self.table_area = area;
        self.table_rows = window
            .iter()
            .map(|(slot, _)| match slot {
                Slot::Top => Slot::Top,
                Slot::Cutoff => Slot::Cutoff,
                Slot::Row(id) => Slot::Row((*id).clone()),
            })
            .collect();
    }

    fn table_row(
        &self,
        id: &CandidateId,
        cutoff: usize,
        columns: &[Col],
        widths: &[u16],
    ) -> TableRow<'static> {
        let model = &self.model;
        let now = model.now;
        let Some(row) = model.rows.get(id) else {
            return TableRow::default();
        };
        let above = model.selection.position(id).is_some_and(|p| p < cutoff);
        let candidate = &row.candidate;
        // Below the cutoff this is the disabled, would-be state.
        let checked = model.selection.is_marked(id, &model.rows);
        let usage_prefix = match candidate.usage {
            Obs::Stale { .. } => "~",
            Obs::Partial { .. } => "≥",
            _ => "",
        };
        let git = candidate.git.value();

        let cells = columns.iter().zip(widths).map(|(column, width)| {
            let width = usize::from(*width);
            let text = match column {
                Col::Mark => match &row.delete {
                    Some(DeleteState::Queued { .. }) => "[q]",
                    Some(DeleteState::Validating) => "[v]",
                    Some(DeleteState::Removing) => "[~]",
                    Some(DeleteState::Finished(Outcome::Removed { .. })) => "[✓]",
                    _ if row.eligibility.is_err() => " - ",
                    _ if checked => "[x]",
                    _ => "[ ]",
                }
                .to_owned(),
                Col::Repo => truncate_end(&candidate.seed.repo_label, width),
                Col::Branch => truncate_end(&candidate.head_label(), width),
                Col::Commit => {
                    let sha = git
                        .and_then(|git| git.head.oid())
                        .map_or("", |oid| &oid[..oid.len().min(7)]);
                    let subject = git.and_then(|git| git.subject.as_deref()).unwrap_or("");
                    truncate_end(format!("{sha} {subject}").trim(), width)
                }
                Col::Path => truncate_middle(&display_path(&candidate.seed.path), width),
                Col::Rate => match &row.rating {
                    Some(rating) if rating.uncertain => format!("~{:.0}", rating.score),
                    Some(rating) => format!("{:.0}", rating.score),
                    None => "–".into(),
                },
                Col::Used => match candidate.last_use() {
                    Some(time) if candidate.usage.is_settled() => format::age(now - time),
                    Some(time) => format!("{usage_prefix}{}", format::age(now - time)),
                    None => "?".into(),
                },
                Col::Age => git
                    .and_then(|git| git.last_commit)
                    .map_or("?".into(), |time| format::age(now - time)),
                Col::Size => match candidate.size() {
                    Some(size) => format!("{usage_prefix}{}", format::size(size)),
                    None if candidate.usage.is_settled() || !self.will_measure(id) => "–".into(),
                    None => "…".into(),
                },
                Col::State => truncate_end(&state_text(row), width),
            };
            Cell::from(text)
        });

        // Rows below the cutoff use a muted palette throughout; explicit
        // colours rather than DIM, which many terminals ignore.
        let style = if row.is_removed() {
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT)
        } else {
            let color = match (above, row.eligibility.is_ok(), checked) {
                (true, true, true) => Some(Color::Green),
                (true, true, false) => None,
                (true, false, _) => Some(Color::DarkGray),
                (false, true, true) => Some(Color::Indexed(65)),
                (false, true, false) => Some(Color::Indexed(244)),
                (false, false, _) => Some(Color::Indexed(239)),
            };
            color.map_or_else(Style::new, |color| Style::new().fg(color))
        };
        TableRow::new(cells).style(style)
    }

    fn draw_details(&self, frame: &mut Frame, area: Rect, focus: Option<&CandidateId>) {
        let lines = focus
            .and_then(|id| self.model.rows.get(id))
            .map(|row| details(row, self.model.now))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().title("details"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, visible: &[CandidateId]) {
        let model = &self.model;
        let selected = model.selection.selected(&model.rows);
        let bytes: u64 = selected
            .iter()
            .filter_map(|id| model.rows.get(id)?.candidate.size())
            .sum();
        let visible: HashSet<&CandidateId> = visible.iter().collect();
        let hidden = selected.iter().filter(|id| !visible.contains(id)).count();

        let mut status = vec![
            Span::from(format!(
                "selected {} ({})",
                selected.len(),
                format::size(bytes)
            ))
            .bold(),
            Span::from(format!(
                " · cutoff {}/{}",
                model.selection.cutoff(),
                model.selection.len()
            )),
        ];
        if hidden > 0 {
            status.push(Span::from(format!(" · {hidden} selected hidden by filter")).yellow());
        }
        if !self.editing_filter && !self.filter.value().is_empty() {
            status.push(Span::from(format!(" · filter: {}", self.filter.value())).cyan());
        }
        if let Some((at, message)) = &self.message
            && at.elapsed() < MESSAGE_TTL
        {
            status.push(Span::from(format!("  {message}")).yellow());
        }

        let keys = if self.help {
            "j/k move · J/K cutoff · = cutoff here · space check · D delete selected · xx delete now · c cancel · / filter · r rerank · R reset · ⏎ details · C hide removed · p pause sizes · q quit · mouse: click row, click box to check, drag cutoff, wheel"
        } else {
            "space check · J/K cutoff · D delete selected · xx delete now · / filter · ⏎ details · ? keys · q quit"
        };
        let [status_area, keys_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        frame.render_widget(Paragraph::new(Line::from(status)), status_area);
        if self.editing_filter {
            crate::text_input::draw(
                frame,
                keys_area,
                Line::from("filter: ").cyan(),
                &self.filter,
            );
        } else {
            frame.render_widget(Paragraph::new(Line::from(keys).dim()), keys_area);
        }
    }
}

pub(super) fn delete_text(state: &DeleteState) -> String {
    match state {
        DeleteState::Queued { batch } => format!("queued (batch {batch})"),
        DeleteState::Validating => "validating".into(),
        DeleteState::Removing => "removing".into(),
        DeleteState::Finished(Outcome::Removed { .. }) => "removed".into(),
        DeleteState::Finished(Outcome::Cancelled) => "cancelled".into(),
        DeleteState::Finished(Outcome::Blocked { reason }) => format!("blocked: {reason}"),
        DeleteState::Finished(Outcome::Failed { reason, .. }) => format!("failed: {reason}"),
    }
}

pub(super) fn state_text(row: &Row) -> String {
    if let Some(state) = &row.delete {
        return delete_text(state);
    }
    if let Err(block) = &row.eligibility {
        return block.short().into();
    }
    let Some(git) = row.candidate.git.complete() else {
        return String::new();
    };
    let mut parts = Vec::new();
    match &git.integration {
        Integration::Merged { .. } => parts.push("merged".to_owned()),
        Integration::ChangesPresent { .. } => parts.push("squashed".to_owned()),
        Integration::NotIntegrated { ahead, .. } => parts.push(format!("+{ahead}")),
        Integration::IsBase => parts.push("base".to_owned()),
        Integration::NoBase => parts.push("no base".to_owned()),
        Integration::Unborn => parts.push("empty".to_owned()),
    }
    if git.dirty.is_some_and(|dirty| !dirty.is_clean()) {
        parts.push("dirty".to_owned());
    }
    if row.candidate.seed.dir == DirState::Missing {
        parts.push("missing dir".to_owned());
    }
    parts.join(" ")
}

fn details(row: &Row, now: i64) -> Vec<Line<'static>> {
    let candidate = &row.candidate;
    let seed = &candidate.seed;
    let mut lines = vec![
        Line::from(display_path(&seed.path)).bold(),
        Line::from(format!("repo {}  ({:?})", seed.repo_label, seed.kind)),
        Line::from(format!("id {}", seed.id)).dim(),
    ];
    let age = |time: Option<i64>| time.map_or("unknown".into(), |time| format::ago(now - time));

    lines.push(Line::from(""));
    match &candidate.git {
        Obs::Complete { value, .. } | Obs::Stale { value, .. } | Obs::Partial { value } => {
            let stale = if matches!(candidate.git, Obs::Stale { .. }) {
                " (cached)"
            } else {
                ""
            };
            lines.push(Line::from(format!(
                "head {}{stale}",
                candidate.head_label()
            )));
            if let Some(subject) = &value.subject {
                let sha = value.head.oid().map_or("", |oid| &oid[..oid.len().min(10)]);
                lines.push(Line::from(format!("{sha} {subject}")));
            }
            lines.push(Line::from(format!(
                "integration: {}",
                format::integration(&value.integration)
            )));
            lines.push(Line::from(format!(
                "last commit {}",
                age(value.last_commit)
            )));
            match value.dirty {
                Some(dirty) if dirty.is_clean() => lines.push(Line::from("working tree clean")),
                Some(dirty) => lines.push(Line::from(format!(
                    "dirty: {} changed, {} untracked, {} conflicted",
                    dirty.changed, dirty.untracked, dirty.conflicted
                ))),
                None if seed.dir == DirState::Missing => {
                    lines.push(Line::from("no working directory"))
                }
                None => lines.push(Line::from("working tree status not inspected")),
            }
        }
        Obs::Pending => lines.push(Line::from("git assessment pending")),
        Obs::Failed { error } => {
            lines.push(Line::from(format!("git assessment failed: {error}")).red())
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "git activity {}",
        age(seed.git_activity)
    )));
    match &candidate.usage {
        Obs::Pending => lines.push(Line::from("measurement pending")),
        Obs::Failed { error } => lines.push(Line::from(format!("not measured: {error}"))),
        Obs::Complete { value, .. } | Obs::Stale { value, .. } | Obs::Partial { value } => {
            let note = match candidate.usage {
                Obs::Stale { .. } => " (cached)",
                Obs::Partial { .. } => " (in progress)",
                _ => "",
            };
            lines.push(Line::from(format!(
                "files modified {}{note}",
                age(value.newest_mtime)
            )));
            lines.push(Line::from(format!(
                "size {} allocated, {} logical, {} entries",
                format::size(value.allocated),
                format::size(value.logical),
                value.entries
            )));
            if value.hardlinked > 0 {
                lines.push(Line::from(format!(
                    "{} in hard-linked files may not be freed",
                    format::size(value.hardlinked)
                )));
            }
            if value.skipped_mounts > 0 || value.errors > 0 {
                lines.push(Line::from(format!(
                    "skipped {} mount points, {} unreadable entries",
                    value.skipped_mounts, value.errors
                )));
            }
        }
    }

    lines.push(Line::from(""));
    match &row.eligibility {
        Ok(()) => lines.push(Line::from("removable").green()),
        Err(block) => lines.push(Line::from(format!("blocked: {block}")).red()),
    }
    if row.eligibility.is_ok() {
        if row.recommendation.checked {
            lines.push(Line::from("recommended for removal"));
        } else {
            for reason in &row.recommendation.reasons {
                lines.push(Line::from(format!("not recommended: {reason}")));
            }
        }
    }
    if let Some(rating) = &row.rating {
        let uncertain = if rating.uncertain {
            " (incomplete evidence)"
        } else {
            ""
        };
        lines.push(Line::from(format!("rating {:.0}{uncertain}", rating.score)));
        for part in &rating.parts {
            lines.push(
                Line::from(format!(
                    "  {} {:.2}×{:.2}  {}",
                    part.name, part.value, part.weight, part.note
                ))
                .dim(),
            );
        }
    }
    if let Some(state) = &row.delete {
        lines.push(Line::from(""));
        lines.push(Line::from(delete_text(state)).magenta());
        if let DeleteState::Finished(Outcome::Removed { report } | Outcome::Failed { report, .. }) =
            state
        {
            lines.push(Line::from(format!("directory: {}", report.directory)));
            lines.push(Line::from(format!("registration: {}", report.registration)));
            lines.push(Line::from(format!("branch: {}", report.branch)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_informative_ends() {
        assert_eq!(truncate_end("feature/ABC-123-long", 10), "feature/A…");
        assert_eq!(truncate_end("short", 10), "short");
        assert_eq!(truncate_end("漢字abc", 4), "漢…");
        assert_eq!(truncate_end("e\u{301}abc", 3), "e\u{301}a…");
        assert_eq!(truncate_end("👨‍👩‍👧‍👦abc", 3), "👨‍👩‍👧‍👦…");
        assert_eq!(truncate_middle("漢字abc", 4), "…abc");
        assert_eq!(truncate_end("abc", 0), "");
        let path = "~/work/bb/sub/sub.feature-SUB-1313";
        let shown = truncate_middle(path, 22);
        assert_eq!(shown.width(), 22);
        assert!(
            shown.starts_with("~/work") && shown.ends_with("SUB-1313"),
            "{shown}"
        );
    }

    #[test]
    fn narrow_terminals_drop_low_priority_columns_first() {
        let long = |_| 100;
        let (wide, widths) = columns_for(200, long);
        assert_eq!(wide.len(), COLUMNS.len());
        assert!(widths.iter().sum::<u16>() + 9 <= 200);

        let (medium, widths) = columns_for(100, long);
        assert!(!medium.contains(&Col::Path));
        let commit = medium
            .iter()
            .position(|column| *column == Col::Commit)
            .unwrap();
        assert!(widths[commit] >= 24, "{widths:?}");
        assert!(widths.iter().sum::<u16>() as usize + medium.len() - 1 <= 100);

        let (narrow, _) = columns_for(70, long);
        assert!(narrow.contains(&Col::Repo) && narrow.contains(&Col::State));
    }
}
