//! The main view: one quiet list of leases, grouped by what to do about them.

use std::collections::HashSet;

use jiff::Timestamp;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{ACCENT, App, Excerpt, GAP, dim, failed, hints_width, key_hints, title, tone};
use crate::lease::{Group, Lease};
use crate::snapshot::Snapshot;
use crate::view::{self, Tone};

#[derive(Default)]
pub(super) struct List {
    selected: Option<String>,
    /// Rows whose default expansion the user flipped with enter.
    toggled: HashSet<String>,
    scroll: u16,
}

impl List {
    /// Keeps the same lease selected across refreshes, by name.
    pub fn keep_selection(&mut self, snapshot: Option<&Snapshot>) {
        let rows = selectable(snapshot);
        let still_there = self
            .selected
            .as_ref()
            .is_some_and(|name| rows.iter().any(|l| &l.name == name));
        if !still_there {
            self.selected = rows.first().map(|l| l.name.clone());
        }
    }

    pub fn key(&mut self, code: KeyCode, snapshot: Option<&Snapshot>) {
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1, snapshot),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1, snapshot),
            KeyCode::Char('g') | KeyCode::Home => self.step(isize::MIN, snapshot),
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX, snapshot),
            KeyCode::Enter => {
                if let Some(name) = self.selected.clone()
                    && !self.toggled.remove(&name)
                {
                    self.toggled.insert(name);
                }
            }
            _ => {}
        }
    }

    fn step(&mut self, by: isize, snapshot: Option<&Snapshot>) {
        let names: Vec<&str> = selectable(snapshot)
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        if names.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_deref()
            .and_then(|name| names.iter().position(|n| *n == name))
            .unwrap_or(0);
        let next = current.saturating_add_signed(by).min(names.len() - 1);
        self.selected = Some(names[next].to_string());
    }

    pub fn selected_lease<'a>(&self, snapshot: Option<&'a Snapshot>) -> Option<&'a Lease> {
        let name = self.selected.as_deref()?;
        snapshot?.leases.iter().find(|l| l.name == name)
    }

    /// Problems open by default; enter flips any row.
    pub fn expanded(&self, lease: &Lease) -> bool {
        (!lease.reasons.is_empty()) != self.toggled.contains(&lease.name)
    }

    /// Scrolls just enough to keep the selected row and its details in view.
    pub fn scroll_to(&mut self, selected: Option<(usize, usize)>, height: usize) -> u16 {
        if let Some((top, bottom)) = selected {
            let scroll = self.scroll as usize;
            if top < scroll {
                self.scroll = top as u16;
            } else if bottom > scroll + height {
                self.scroll = bottom.saturating_sub(height) as u16;
            }
            // Keep the group heading in view when the first row is selected.
            if top <= 1 {
                self.scroll = 0;
            }
        }
        self.scroll
    }
}

fn selectable(snapshot: Option<&Snapshot>) -> Vec<&Lease> {
    snapshot
        .iter()
        .flat_map(|s| &s.leases)
        .filter(|l| l.group != Group::DatabaseOnly)
        .collect()
}

pub(super) fn header(app: &App) -> Line<'static> {
    let mut spans = vec![title("crumb".to_string()), Span::raw("  ")];
    match &app.snapshot {
        None => spans.push(Span::styled(
            format!("connecting to {}…", app.host.label()),
            dim(),
        )),
        Some(snapshot) => {
            let mut parts = vec![snapshot.host.clone()];
            if let Some(mem) = snapshot.mem {
                parts.push(format!("{:.1} GB free", mem.available_mb as f64 / 1024.0));
            }
            let count = selectable(Some(snapshot)).len();
            parts.push(format!(
                "{count} lease{}",
                if count == 1 { "" } else { "s" }
            ));
            spans.push(Span::styled(parts.join(" · "), dim()));
        }
    }
    if let Some(error) = &app.error {
        let first = error.lines().next().unwrap_or_default();
        // The list below is the last good snapshot; say how old it is.
        let shown = app.snapshot.as_ref().map(|s| {
            let at =
                s.at.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%H:%M");
            format!(" · showing {at}")
        });
        spans.push(Span::styled(
            format!(" · {first}{}", shown.unwrap_or_default()),
            tone(Tone::Bad),
        ));
    }
    Line::from(spans)
}

/// The list, and the line range of the selected row with its details.
pub(super) fn body(app: &App) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    let Some(snapshot) = &app.snapshot else {
        if let Some(error) = &app.error {
            lines.push(Line::styled(error.clone(), tone(Tone::Bad)));
        }
        return (lines, None);
    };
    if snapshot.leases.is_empty() {
        lines.push(Line::styled(
            format!("no leases on {}", snapshot.host),
            dim(),
        ));
    }

    let now = Timestamp::now();
    let rows: Vec<(&Lease, view::Row)> = selectable(Some(snapshot))
        .into_iter()
        .map(|l| {
            let mut row = view::row(l, now);
            if let Some(activity) = app.busy.get(&l.name) {
                row.status = activity.status();
            }
            (l, row)
        })
        .collect();
    let widths = view::widths(rows.iter().map(|(_, row)| row));
    let mut groups: Vec<Group> = snapshot.leases.iter().map(|l| l.group).collect();
    groups.dedup();
    let headed = groups.len() > 1;

    let mut selected = None;
    for (index, group) in groups.iter().enumerate() {
        let members: Vec<&Lease> = snapshot
            .leases
            .iter()
            .filter(|l| l.group == *group)
            .collect();
        if index > 0 {
            lines.push(Line::default());
        }
        if headed {
            lines.push(heading(*group, &members));
        }
        if *group == Group::DatabaseOnly {
            let names: Vec<&str> = members
                .iter()
                .map(|l| {
                    l.database
                        .as_ref()
                        .map_or(l.name.as_str(), |d| d.name.as_str())
                })
                .collect();
            lines.push(Line::styled(format!("  {}", names.join(GAP)), dim()));
            continue;
        }
        for (lease, row) in rows.iter().filter(|(l, _)| l.group == *group) {
            let is_selected = app.list.selected.as_deref() == Some(lease.name.as_str());
            let top = lines.len();
            lines.push(row_line(row, widths, is_selected));
            let busy = app.busy.contains_key(&lease.name);
            if is_selected && (app.list.expanded(lease) || busy) {
                lines.extend(details(app, lease, now));
            }
            if is_selected {
                selected = Some((top, lines.len()));
            }
        }
    }
    for warning in &snapshot.warnings {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("warning  ", tone(Tone::Warn)),
            Span::styled(warning.clone(), dim()),
        ]));
    }
    (lines, selected)
}

/// The lines under an expanded row: why it needs attention, then the last
/// error lines from its backend when it has failed.
fn details(app: &App, lease: &Lease, now: Timestamp) -> Vec<Line<'static>> {
    // Long reasons wrap under their own column rather than off the screen.
    let columns = app.body_width.saturating_sub(14);
    let detail = |label: &str, text: String, t: Tone| -> Vec<Line<'static>> {
        view::wrap(&text, columns)
            .into_iter()
            .enumerate()
            .map(|(index, part)| {
                let label = if index == 0 { label } else { "" };
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(view::pad(label, 10), dim()),
                    Span::styled(part, tone(t)),
                ])
            })
            .collect()
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(activity) = app.busy.get(&lease.name) {
        match &activity.error {
            Some(error) => {
                for (index, line) in error.lines().enumerate() {
                    let label = if index == 0 { activity.verb } else { "" };
                    lines.extend(detail(label, line.trim().to_string(), Tone::Bad));
                }
                lines.extend(detail(
                    "",
                    "esc dismisses · c shows every command".into(),
                    Tone::Dim,
                ));
            }
            None => {
                let recent = activity.lines.len().saturating_sub(3);
                for (index, line) in activity.lines[recent..].iter().enumerate() {
                    let label = if index == 0 { activity.verb } else { "" };
                    lines.extend(detail(label, line.clone(), Tone::Dim));
                }
            }
        }
        return lines;
    }
    lines.extend(
        view::reasons(lease, &app.config, now)
            .into_iter()
            .flat_map(|(label, text, t)| detail(label, text, t)),
    );
    if !failed(lease) {
        return lines;
    }
    match app.excerpt_for(lease) {
        Some(Excerpt::Loading) => lines.extend(detail("logs", "reading…".into(), Tone::Dim)),
        Some(Excerpt::Lines(excerpt)) => {
            for (index, line) in excerpt.iter().enumerate() {
                let t = if view::is_error_line(line) {
                    Tone::Bad
                } else {
                    Tone::Dim
                };
                let label = if index == 0 { "logs" } else { "" };
                lines.extend(detail(label, line.clone(), t));
            }
        }
        Some(Excerpt::Failed(err)) => {
            lines.extend(detail("logs", format!("couldn't read: {err}"), Tone::Dim));
        }
        None => {}
    }
    lines
}

pub(super) fn keys(app: &App) -> Line<'static> {
    let lease = app.selected_lease();
    let details = match lease {
        Some(lease) if app.list.expanded(lease) => "collapse",
        _ => "details",
    };
    let mut pairs = vec![("↑↓", "move"), ("⏎", details)];
    if let Some(lease) = lease
        && !app.busy.get(&lease.name).is_some_and(|a| !a.done)
    {
        pairs.extend(app.verbs(lease));
    }
    if lease.is_some_and(|l| crate::ops::log_command(&app.config, l, false, 1).is_some()) {
        pairs.push(("l", "logs"));
    }
    pairs.extend([("c", "commands"), ("q", "quit")]);
    if let Some(notice) = app.notice {
        pairs.push(("·", notice));
    }
    // Keys every screen has go first when the footer runs out of room.
    for key in ["↑↓", "c", "⏎", "q"] {
        if hints_width(&pairs) <= app.body_width {
            break;
        }
        pairs.retain(|(k, _)| *k != key);
    }
    key_hints(&pairs)
}

fn heading(group: Group, members: &[&Lease]) -> Line<'static> {
    match group {
        Group::Orphaned => {
            let mut spans = vec![Span::styled("orphaned", tone(Tone::Warn))];
            if let Some(total) = view::total_memory(members.iter().copied()) {
                spans.push(Span::styled(format!(" · {}", view::memory(total)), dim()));
            }
            spans.push(Span::raw(GAP));
            spans.push(Span::styled("R", Style::new().fg(ACCENT)));
            spans.push(Span::styled(" reap", dim()));
            Line::from(spans)
        }
        _ => Line::styled(view::group_title(group), dim()),
    }
}

fn row_line(row: &view::Row, widths: view::Widths, selected: bool) -> Line<'static> {
    let quiet = |t: Tone| if row.quiet { Tone::Dim } else { t };
    let status_tone = match row.status.1 {
        Tone::Bad => Tone::Bad,
        t => quiet(t),
    };
    let marker = if selected {
        Span::styled("▌ ", Style::new().fg(ACCENT))
    } else {
        Span::raw("  ")
    };
    Line::from(vec![
        marker,
        Span::styled(view::pad(&row.name, widths.name), tone(quiet(Tone::Normal))),
        Span::raw(GAP),
        Span::styled(
            view::pad(&row.worktree.0, widths.worktree),
            tone(quiet(row.worktree.1)),
        ),
        Span::raw(GAP),
        Span::styled(view::pad(&row.port, widths.port), tone(quiet(Tone::Normal))),
        Span::raw(GAP),
        Span::styled(view::pad(&row.status.0, widths.status), tone(status_tone)),
        Span::raw(GAP),
        Span::styled(row.memory.clone(), dim()),
    ])
}
