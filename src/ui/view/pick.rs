//! Picker rendering. Records which rows the frame showed, so input acts on
//! what the user saw.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row as TableRow, Table};
use unicode_width::UnicodeWidthStr;

use crate::core::format::{self, truncate_end};
use crate::pick::engine::Control;
use crate::pick::listing::Target;
use crate::ui::input::follow;
use crate::ui::pick_app::PickApp;

use super::widgets::text_input;

pub fn draw<C: Control>(app: &mut PickApp<C>, frame: &mut Frame) {
    let [header, input, list, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    app.list_area = list;

    frame.render_widget(Paragraph::new(header_line(app)), header);
    draw_input(app, frame, input);
    draw_list(app, frame, list);
    frame.render_widget(Paragraph::new(footer_line(app)), footer);
}

fn header_line<C: Control>(app: &PickApp<C>) -> Line<'static> {
    let mut spans = vec![Span::from("git-yard pick").bold(), Span::from("  ")];
    if app.discovery_done {
        spans.push(Span::from(format!("{} projects", app.projects)));
    } else {
        spans.push(Span::from(format!("discovering… {}", app.fresh.len())).cyan());
    }
    spans.push(Span::from(format!(" · {} shown", app.results.len())).dim());
    if !app.fetching.is_empty() {
        spans.push(Span::from(format!(" · fetching {}", app.fetching.len())).cyan());
    }
    if !app.querying.is_empty() {
        spans.push(Span::from(format!(" · requests {}", app.querying.len())).cyan());
    }
    if !app.errors.is_empty() {
        spans.push(Span::from(format!(" · {} errors", app.errors.len())).red());
    }
    Line::from(spans)
}

fn draw_input<C: Control>(app: &PickApp<C>, frame: &mut Frame, area: Rect) {
    if let Some(name) = &app.prompt {
        let base = app
            .selected()
            .map(|item| item.branch_text())
            .unwrap_or_default();
        let prefix = Line::from(format!("new branch from {base}: ")).yellow();
        text_input(frame, area, prefix, name);
    } else {
        let mut spans = Vec::new();
        if let Some(scope) = &app.scope {
            spans.push(Span::from(format!("[{}] ", app.label(scope))).cyan());
        }
        spans.push(Span::from("> ").bold());
        text_input(frame, area, Line::from(spans), &app.query);
    }
}

fn footer_line<C: Control>(app: &PickApp<C>) -> Line<'static> {
    if let Some(message) = app.toast.current() {
        return Line::from(Span::from(message.to_owned()).yellow());
    }
    if app.prompt.is_some() {
        return Line::from("enter create worktree · esc cancel").dim();
    }
    let last_error = app
        .errors
        .last()
        .map(|error| format!(" · last error: {error}"));
    Line::from(vec![
        Span::from(
            "enter open · alt-enter print path · tab project · ctrl-o new branch · esc back",
        )
        .dim(),
        Span::from(last_error.unwrap_or_default()).red().dim(),
    ])
}

fn draw_list<C: Control>(app: &mut PickApp<C>, frame: &mut Frame, area: Rect) {
    let height = usize::from(area.height);
    app.offset = follow(app.offset, app.cursor, height, app.results.len(), 0);
    let start = app.offset.min(app.results.len());
    let shown: Vec<usize> = app.results[start..].iter().take(height).copied().collect();

    // Widths follow the longest content among the first rows of the
    // ranking, so they stay put while scrolling.
    let (mut project_width, mut branch_width) = (0, 0);
    for &index in app.results.iter().take(500) {
        let item = &app.items[index];
        project_width = project_width.max(item.project.label.width());
        branch_width = branch_width.max(item.branch_text().width());
    }
    let available = usize::from(area.width).saturating_sub(2 + 5 + 3);
    let project_width = project_width.clamp(8, 30).min(available * 3 / 10);
    let branch_width = branch_width.clamp(8, 45).min(available * 35 / 100);
    let info_width = available.saturating_sub(project_width + branch_width);

    let rows: Vec<TableRow> = shown
        .iter()
        .enumerate()
        .map(|(row, &index)| {
            let item = &app.items[index];
            let selected = start + row == app.cursor;
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
                .map_or(String::new(), |time| format::age(app.now - time));
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
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Length(project_width as u16),
        Constraint::Length(branch_width as u16),
        Constraint::Length(info_width as u16),
        Constraint::Length(4),
    ];
    frame.render_widget(Table::new(rows, widths).column_spacing(1), area);

    app.drawn = shown
        .iter()
        .map(|&index| app.items[index].key.clone())
        .collect();
}
