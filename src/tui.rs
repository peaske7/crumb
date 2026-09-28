use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use jiff::{Timestamp, Zoned};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::lease::{Group, Lease};
use crate::run::Runner;
use crate::snapshot::{self, Snapshot};
use crate::view::{self, Tone};

const REFRESH: Duration = Duration::from_secs(3);
const GAP: &str = "   ";
const ACCENT: Color = Color::Blue;

enum Msg {
    Input(Event),
    Snapshot(Box<Result<Snapshot>>),
}

pub fn run(config: Config) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    spawn_refresh(config.clone(), Runner::default(), tx.clone());
    spawn_input(tx);
    let mut terminal = ratatui::init();
    let result = App::new(config.host.label().to_string()).run(&mut terminal, &rx);
    ratatui::restore();
    result
}

/// Reads on its own thread so the screen never waits on the network.
fn spawn_refresh(config: Config, runner: Runner, tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let result = snapshot::collect(&config, &runner);
            if tx.send(Msg::Snapshot(Box::new(result))).is_err() {
                break;
            }
            thread::sleep(REFRESH);
        }
    });
}

fn spawn_input(tx: Sender<Msg>) {
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(Msg::Input(event)).is_err() {
                break;
            }
        }
    });
}

struct App {
    host: String,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    selected: Option<String>,
    /// Rows whose default expansion the user flipped with enter.
    toggled: HashSet<String>,
    scroll: u16,
    quit: bool,
}

impl App {
    fn new(host: String) -> Self {
        Self {
            host,
            snapshot: None,
            error: None,
            selected: None,
            toggled: HashSet::new(),
            scroll: 0,
            quit: false,
        }
    }

    /// Redraws only when a key or new data arrives.
    fn run(mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<()> {
        terminal.draw(|frame| self.draw(frame))?;
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => self.key(key),
                Msg::Input(_) => {}
                Msg::Snapshot(result) => self.update(*result),
            }
            if self.quit {
                break;
            }
            terminal.draw(|frame| self.draw(frame))?;
        }
        Ok(())
    }

    fn update(&mut self, result: Result<Snapshot>) {
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.error = None;
                let selectable = self.selectable();
                let still_there = self
                    .selected
                    .as_ref()
                    .is_some_and(|name| selectable.iter().any(|l| &l.name == name));
                if !still_there {
                    self.selected = selectable.first().map(|l| l.name.clone());
                }
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('g') | KeyCode::Home => self.step(isize::MIN),
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX),
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

    fn step(&mut self, by: isize) {
        let names: Vec<String> = self.selectable().iter().map(|l| l.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name))
            .unwrap_or(0);
        let next = current.saturating_add_signed(by).min(names.len() - 1);
        self.selected = Some(names[next].clone());
    }

    fn selectable(&self) -> Vec<&Lease> {
        self.snapshot
            .iter()
            .flat_map(|s| &s.leases)
            .filter(|l| l.group != Group::DatabaseOnly)
            .collect()
    }

    /// Problems open by default; enter flips any row.
    fn expanded(&self, lease: &Lease) -> bool {
        (!lease.reasons.is_empty()) != self.toggled.contains(&lease.name)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let area = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        };
        let [header, _, body, _, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(self.header()), header);
        let clock = Zoned::now().strftime("%H:%M").to_string();
        frame.render_widget(
            Paragraph::new(Span::styled(clock, dim())).right_aligned(),
            header,
        );

        let (lines, selected) = self.body();
        let height = body.height as usize;
        if let Some((top, bottom)) = selected {
            let scroll = self.scroll as usize;
            if top < scroll {
                self.scroll = top as u16;
            } else if bottom > scroll + height {
                self.scroll = (bottom - height) as u16;
            }
            // Keep the group heading in view when the first row is selected.
            if top <= 1 {
                self.scroll = 0;
            }
        }
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body);
        frame.render_widget(Paragraph::new(self.footer()), footer);
    }

    fn header(&self) -> Line<'static> {
        let mut spans = vec![
            Span::styled("crumb", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
        ];
        match &self.snapshot {
            None => spans.push(Span::styled(format!("connecting to {}…", self.host), dim())),
            Some(snapshot) => {
                let mut parts = vec![snapshot.host.clone()];
                if let Some(mem) = snapshot.mem {
                    parts.push(format!("{:.1} GB free", mem.available_mb as f64 / 1024.0));
                }
                let count = snapshot
                    .leases
                    .iter()
                    .filter(|l| l.group != Group::DatabaseOnly)
                    .count();
                parts.push(format!(
                    "{count} lease{}",
                    if count == 1 { "" } else { "s" }
                ));
                spans.push(Span::styled(parts.join(" · "), dim()));
            }
        }
        if let Some(error) = &self.error {
            let first = error.lines().next().unwrap_or_default();
            // The list below is the last good snapshot; say how old it is.
            let shown = self.snapshot.as_ref().map(|s| {
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

    /// The list, and the line range of the selected row with its expansion.
    fn body(&self) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let mut lines: Vec<Line> = Vec::new();
        let Some(snapshot) = &self.snapshot else {
            if let Some(error) = &self.error {
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
        let rows: Vec<(&Lease, view::Row)> = snapshot
            .leases
            .iter()
            .filter(|l| l.group != Group::DatabaseOnly)
            .map(|l| (l, view::row(l, now)))
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
                let is_selected = self.selected.as_deref() == Some(lease.name.as_str());
                let top = lines.len();
                lines.push(row_line(row, widths, is_selected));
                if is_selected && self.expanded(lease) {
                    for (label, text, t) in view::reasons(lease, now) {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(view::pad(label, 10), dim()),
                            Span::styled(text, tone(t)),
                        ]));
                    }
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

    fn footer(&self) -> Line<'static> {
        let details = match self.selected_lease() {
            Some(lease) if self.expanded(lease) => "collapse",
            _ => "details",
        };
        let keys = [("↑↓", "move"), ("⏎", details), ("q", "quit")];
        let mut spans = Vec::new();
        for (index, (key, label)) in keys.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(GAP));
            }
            spans.push(Span::styled(key, Style::new().fg(ACCENT)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, dim()));
        }
        Line::from(spans)
    }

    fn selected_lease(&self) -> Option<&Lease> {
        let name = self.selected.as_deref()?;
        self.snapshot
            .as_ref()?
            .leases
            .iter()
            .find(|l| l.name == name)
    }
}

fn heading(group: Group, members: &[&Lease]) -> Line<'static> {
    match group {
        Group::Orphaned => {
            let mut spans = vec![Span::styled("orphaned", tone(Tone::Warn))];
            if let Some(total) = view::total_memory(members.iter().copied()) {
                spans.push(Span::styled(format!(" · {}", view::memory(total)), dim()));
            }
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

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Colors come from the terminal's own 16-color palette, so crumb follows its theme.
fn tone(t: Tone) -> Style {
    match t {
        Tone::Normal => Style::new(),
        Tone::Dim => dim(),
        Tone::Warn => Style::new().fg(Color::Yellow),
        Tone::Bad => Style::new().fg(Color::Red),
    }
}
