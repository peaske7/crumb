//! The TUI: one list, plus full-screen logs and command-log views you leave
//! with `esc`. All I/O runs on background threads; the screen redraws only
//! when a key or new data arrives.

mod actions;
mod commands;
mod list;
mod logs;

use std::collections::HashMap;
use std::io::Write;
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

use actions::{Activity, Answer, Pending, TuiProgress};

use crate::checks::Images;
use crate::config::{Config, Forward, Host};
use crate::lease::{Group, Lease};
use crate::ops::{self, Ctx};
use crate::run::Runner;
use crate::snapshot::{self, Here, Snapshot};
use crate::view::{self, Tone};
use crate::worktree::Checkout;

const REFRESH: Duration = Duration::from_secs(3);
const GAP: &str = "   ";
const ACCENT: Color = Color::Blue;

enum Msg {
    Input(Event),
    Snapshot(Box<Result<Snapshot>>),
    LogLine {
        generation: u64,
        line: String,
    },
    Excerpt {
        key: ExcerptKey,
        result: Result<Vec<String>, String>,
    },
    /// A step or output line from a running action.
    Progress {
        lease: String,
        line: String,
        step: bool,
    },
    Done {
        lease: String,
        result: Result<(), String>,
    },
}

pub fn run(
    config: Config,
    runner: Runner,
    checkout: Option<Checkout>,
    here: Option<Here>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (poke, poked) = mpsc::channel();
    spawn_refresh(config.clone(), runner.clone(), tx.clone(), poked, here);
    spawn_input(tx.clone());
    let mut terminal = ratatui::init();
    let result = App::new(config, runner, tx, poke, checkout).run(&mut terminal, &rx);
    ratatui::restore();
    result
}

/// Reads on its own thread so the screen never waits on the network. A new
/// image's facts take about a second, so the list is sent first and sent
/// again once they arrive. A poke refreshes at once, as after an action.
fn spawn_refresh(
    config: Config,
    runner: Runner,
    tx: Sender<Msg>,
    poked: Receiver<()>,
    here: Option<Here>,
) {
    thread::spawn(move || {
        let mut images = Images::default();
        let send = |result: Result<Snapshot>| tx.send(Msg::Snapshot(Box::new(result))).is_ok();
        loop {
            let open = match snapshot::gather(&config, &runner) {
                Err(err) => send(Err(err)),
                Ok(facts) => {
                    let first = snapshot::assemble(&config, &facts, &images, here.as_ref());
                    let missing = images.missing(&config, &first.leases);
                    let mut open = send(Ok(first));
                    if open && !missing.is_empty() {
                        for id in &missing {
                            images.fetch(&runner, &config, id);
                        }
                        open = send(Ok(snapshot::assemble(
                            &config,
                            &facts,
                            &images,
                            here.as_ref(),
                        )));
                    }
                    open
                }
            };
            if !open {
                break;
            }
            match poked.recv_timeout(REFRESH) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => while poked.try_recv().is_ok() {},
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
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

enum View {
    List,
    Logs(logs::Logs),
    Commands(commands::Commands),
    Confirm(Pending),
}

/// Error lines under a failed lease are fetched once per container run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExcerptKey {
    container: String,
    since: Option<Timestamp>,
}

enum Excerpt {
    Loading,
    Lines(Vec<String>),
    Failed(String),
}

struct App {
    config: Config,
    host: Host,
    runner: Runner,
    tx: Sender<Msg>,
    poke: Sender<()>,
    checkout: Option<Checkout>,
    /// Actions running or failed, by lease.
    busy: HashMap<String, Activity>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    list: list::List,
    view: View,
    excerpts: HashMap<ExcerptKey, Excerpt>,
    /// Tags log lines so lines from a stream you already left are ignored.
    generation: u64,
    body_height: usize,
    body_width: usize,
    /// Text to hand to the terminal's clipboard after the next draw.
    clipboard: Option<String>,
    notice: Option<&'static str>,
    quit: bool,
}

impl App {
    fn new(
        config: Config,
        runner: Runner,
        tx: Sender<Msg>,
        poke: Sender<()>,
        checkout: Option<Checkout>,
    ) -> Self {
        Self {
            host: config.host.clone(),
            config,
            runner,
            tx,
            poke,
            checkout,
            busy: HashMap::new(),
            snapshot: None,
            error: None,
            list: list::List::default(),
            view: View::List,
            excerpts: HashMap::new(),
            generation: 0,
            body_height: 0,
            body_width: 80,
            clipboard: None,
            notice: None,
            quit: false,
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal, rx: &Receiver<Msg>) -> Result<()> {
        terminal.draw(|frame| self.draw(frame))?;
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => self.key(key),
                Msg::Input(_) => {}
                Msg::Snapshot(result) => self.update(*result),
                Msg::LogLine { generation, line } => {
                    if let View::Logs(logs) = &mut self.view
                        && generation == self.generation
                    {
                        logs.push(line);
                    }
                }
                Msg::Excerpt { key, result } => {
                    let excerpt = match result {
                        Ok(lines) => Excerpt::Lines(lines),
                        Err(err) => Excerpt::Failed(err),
                    };
                    self.excerpts.insert(key, excerpt);
                }
                Msg::Progress { lease, line, step } => {
                    if let Some(activity) = self.busy.get_mut(&lease) {
                        activity.push(line, step);
                    }
                }
                Msg::Done { lease, result } => match result {
                    Ok(()) => {
                        self.busy.remove(&lease);
                    }
                    Err(err) => {
                        if let Some(activity) = self.busy.get_mut(&lease) {
                            activity.done = true;
                            activity.error = Some(err);
                        }
                    }
                },
            }
            if self.quit {
                break;
            }
            self.fetch_excerpt();
            terminal.draw(|frame| self.draw(frame))?;
            if let Some(text) = self.clipboard.take() {
                copy_to_clipboard(terminal, &text)?;
            }
        }
        Ok(())
    }

    fn update(&mut self, result: Result<Snapshot>) {
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.error = None;
                self.list.keep_selection(self.snapshot.as_ref());
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn key(&mut self, key: KeyEvent) {
        self.notice = None;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        let back = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));
        match &mut self.view {
            View::Confirm(pending) => match pending.key(key.code) {
                Answer::Wait => {}
                Answer::Cancel => self.view = View::List,
                Answer::Apply => {
                    let View::Confirm(pending) = std::mem::replace(&mut self.view, View::List)
                    else {
                        unreachable!()
                    };
                    self.apply(pending);
                }
            },
            View::List => match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('l') => self.open_logs(),
                KeyCode::Char('c') => self.view = View::Commands(commands::Commands::default()),
                KeyCode::Char(c @ ('u' | 'r' | 's' | 'd' | 'D' | 'm' | 't')) => self.lifecycle(c),
                KeyCode::Char('R') => self.plan_reap(),
                KeyCode::Esc => {
                    // Dismiss a failed action's message.
                    if let Some(name) = self.selected_lease().map(|l| l.name.clone()) {
                        self.busy.remove(&name);
                    }
                }
                code => self.list.key(code, self.snapshot.as_ref()),
            },
            // Leaving the logs view drops its stream, which stops `docker logs`.
            View::Logs(_) | View::Commands(_) if back => self.view = View::List,
            View::Logs(logs) => logs.key(key.code, self.body_height),
            View::Commands(commands) => {
                let rows = view::fold(&self.runner.records());
                if let Some(text) = commands.key(key.code, &rows) {
                    self.clipboard = Some(text);
                    self.notice = Some("copied");
                }
            }
        }
    }

    fn selected_lease(&self) -> Option<&Lease> {
        self.list.selected_lease(self.snapshot.as_ref())
    }

    /// Which lifecycle keys apply to a lease, in footer order.
    fn verbs(&self, lease: &Lease) -> Vec<(&'static str, &'static str)> {
        let mut keys = Vec::new();
        let live = lease.live_worktree().is_some();
        let has_backend = lease.container.is_some() || lease.sync.is_some();
        if lease.group == Group::Orphaned {
            keys.push(("R", "reap"));
            if has_backend {
                keys.push(("d", "down"));
            }
            return keys;
        }
        if live && !lease.is_running() {
            keys.push(("u", "up"));
        }
        if lease.is_running() || matches!(lease.state, crate::lease::State::CrashLoop { .. }) {
            keys.push(("r", "restart"));
            keys.push(("s", "stop"));
        }
        let fixes_wiring = self.config.forward == Forward::Mutagen || !self.config.wires.is_empty();
        if lease.is_running() && live && fixes_wiring {
            keys.push(("t", "tunnel"));
        }
        if live
            && self
                .config
                .database
                .as_ref()
                .is_some_and(|db| db.migrate.is_some())
            && lease.database.is_some()
        {
            keys.push(("m", "migrate"));
        }
        if has_backend {
            keys.push(("d", "down"));
        }
        if lease.database.is_some() {
            keys.push(("D", "drop"));
        }
        keys
    }

    fn lifecycle(&mut self, key: char) {
        let Some(lease) = self.selected_lease().cloned() else {
            return;
        };
        let allowed = self.verbs(&lease).iter().any(|(k, _)| k.starts_with(key));
        if !allowed || self.busy.get(&lease.name).is_some_and(|a| !a.done) {
            return;
        }
        let name = lease.name.clone();
        match key {
            'u' => {
                let Some(worktree) = lease.live_worktree().map(std::path::PathBuf::from) else {
                    return;
                };
                self.spawn("up", name.clone(), move |ctx| {
                    ops::up(ctx, &name, &worktree)
                });
            }
            'r' => self.spawn("restart", name.clone(), move |ctx| ops::restart(ctx, &name)),
            's' => self.spawn("stop", name.clone(), move |ctx| ops::stop(ctx, &name)),
            'm' => self.spawn("migrate", name.clone(), move |ctx| ops::migrate(ctx, &name)),
            't' => self.spawn("tunnel", name.clone(), move |ctx| ops::tunnel(ctx, &name)),
            'd' => {
                let lines = self.down_plan(&lease);
                self.view = View::Confirm(Pending::Down { lease: name, lines });
            }
            'D' => {
                let mut lines = self.down_plan(&lease);
                lines.pop();
                if let Some(db) = &lease.database {
                    lines.push(format!("drop the database {}", db.name));
                }
                self.view = View::Confirm(Pending::Drop {
                    lease: name,
                    lines,
                    typed: String::new(),
                });
            }
            _ => {}
        }
    }

    /// What `down` will remove, in words.
    fn down_plan(&self, lease: &Lease) -> Vec<String> {
        let mut lines = Vec::new();
        if lease.container.is_some() {
            lines.push(format!(
                "remove the containers of {}",
                self.config.project.render(&lease.name)
            ));
        }
        if let Some(forward) = &lease.forward {
            lines.push(format!("remove the forward {}", forward.session));
        }
        if let Some(sync) = &lease.sync {
            lines.push(format!("remove the sync {} and its replica", sync.session));
        }
        if let Some(worktree) = lease.live_worktree() {
            for w in &self.config.wires {
                if std::path::Path::new(worktree).join(&w.file).is_file() {
                    lines.push(format!("remove {}", w.file));
                }
            }
        }
        match &lease.database {
            Some(db) => lines.push(format!("keep the database {}", db.name)),
            None => lines.push(String::new()),
        }
        lines
    }

    fn plan_reap(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let plan = ops::reap::plan(snapshot, Timestamp::now());
        if plan.items.is_empty() {
            self.notice = Some("no orphans");
            return;
        }
        self.view = View::Confirm(Pending::Reap { plan });
    }

    fn apply(&mut self, pending: Pending) {
        match pending {
            Pending::Down { lease, .. } => {
                let name = lease.clone();
                self.spawn("down", lease, move |ctx| ops::down(ctx, &name));
            }
            Pending::Drop { lease, .. } => {
                let name = lease.clone();
                self.spawn("drop", lease, move |ctx| ops::drop(ctx, &name));
            }
            Pending::Reap { plan } => {
                for item in &plan.items {
                    if item.action != ops::reap::Action::Keep {
                        self.busy
                            .insert(item.lease.name.clone(), Activity::new("reap"));
                    }
                }
                let tx = self.tx.clone();
                self.spawn_with("reap", "reap".to_string(), move |ctx| {
                    let failures = ops::reap::apply(ctx, &plan);
                    for item in &plan.items {
                        let error = failures
                            .iter()
                            .find(|(name, _)| *name == item.lease.name)
                            .map(|(_, err)| err.clone());
                        let _ = tx.send(Msg::Done {
                            lease: item.lease.name.clone(),
                            result: error.map_or(Ok(()), Err),
                        });
                    }
                    Ok(())
                });
            }
        }
    }

    fn spawn(
        &mut self,
        verb: &'static str,
        lease: String,
        work: impl FnOnce(&Ctx) -> Result<()> + Send + 'static,
    ) {
        self.busy.insert(lease.clone(), Activity::new(verb));
        self.spawn_with(verb, lease, work);
    }

    /// Runs an action on its own thread; its steps come back as messages.
    fn spawn_with(
        &mut self,
        _verb: &'static str,
        lease: String,
        work: impl FnOnce(&Ctx) -> Result<()> + Send + 'static,
    ) {
        let config = self.config.clone();
        let runner = self.runner.clone();
        let main = self.checkout.as_ref().map(|c| c.main.clone());
        let (tx, poke) = (self.tx.clone(), self.poke.clone());
        thread::spawn(move || {
            let progress = TuiProgress {
                lease: lease.clone(),
                tx: tx.clone(),
            };
            let ctx = Ctx {
                config: &config,
                runner: &runner,
                out: &progress,
                main: main.as_deref(),
            };
            let result = work(&ctx).map_err(|err| format!("{err:#}"));
            let _ = tx.send(Msg::Done { lease, result });
            let _ = poke.send(());
        });
    }

    fn open_logs(&mut self) {
        let Some(lease) = self.selected_lease() else {
            return;
        };
        let Some(container) = lease.container.clone().filter(|c| safe_name(c)) else {
            return;
        };
        let name = lease.name.clone();
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let stream = self.runner.stream(
            &self.host,
            &format!("docker logs --follow --tail 300 {container} 2>&1"),
            move |line| {
                let _ = tx.send(Msg::LogLine { generation, line });
            },
        );
        self.view = View::Logs(logs::Logs::new(name, stream));
    }

    /// Starts fetching error lines for the selected lease when it has failed
    /// and they are not already known for this run of its container.
    fn fetch_excerpt(&mut self) {
        if !matches!(self.view, View::List) {
            return;
        }
        let Some(lease) = self.selected_lease() else {
            return;
        };
        if !self.list.expanded(lease) || !failed(lease) {
            return;
        }
        let Some(container) = lease.container.clone().filter(|c| safe_name(c)) else {
            return;
        };
        let key = ExcerptKey {
            container: container.clone(),
            since: lease.since,
        };
        if self.excerpts.contains_key(&key) {
            return;
        }
        self.excerpts.insert(key.clone(), Excerpt::Loading);
        let (runner, host, tx) = (self.runner.clone(), self.host.clone(), self.tx.clone());
        thread::spawn(move || {
            let result = runner
                .command(&host, &format!("docker logs --tail 300 {container} 2>&1"))
                .map_err(|err| format!("{err:#}"))
                .and_then(|output| {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let lines: Vec<String> = text.lines().map(view::clean_log_line).collect();
                    match output.status.success() {
                        true => Ok(view::excerpt(&lines, 2)),
                        false => Err(lines.last().cloned().unwrap_or_default()),
                    }
                });
            let _ = tx.send(Msg::Excerpt { key, result });
        });
    }

    fn excerpt_for(&self, lease: &Lease) -> Option<&Excerpt> {
        self.excerpts.get(&ExcerptKey {
            container: lease.container.clone()?,
            since: lease.since,
        })
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
        self.body_height = body.height as usize;
        self.body_width = body.width as usize;

        let clock = Zoned::now().strftime("%H:%M").to_string();
        frame.render_widget(
            Paragraph::new(Span::styled(clock, dim())).right_aligned(),
            header,
        );
        let (head, lines, keys) = match &self.view {
            View::List => {
                let (lines, selected) = list::body(self);
                let scroll = self.list.scroll_to(selected, self.body_height);
                frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
                (list::header(self), None, list::keys(self))
            }
            View::Logs(logs) => {
                let status = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.leases.iter().find(|l| l.name == logs.lease))
                    .map(|l| view::status(l, Timestamp::now()));
                (
                    logs.header(status),
                    Some(logs.lines(self.body_height)),
                    logs.keys(),
                )
            }
            View::Commands(commands) => {
                let rows = view::fold(&self.runner.records());
                (
                    commands::header(),
                    Some(commands.lines(&rows, self.body_height)),
                    commands::keys(self.notice),
                )
            }
            View::Confirm(pending) => (
                pending.header(self.host.label()),
                Some(pending.lines()),
                pending.keys(),
            ),
        };
        frame.render_widget(Paragraph::new(head), header);
        if let Some(lines) = lines {
            frame.render_widget(Paragraph::new(lines), body);
        }
        frame.render_widget(Paragraph::new(keys), footer);
    }
}

/// A lease whose backend has failed, so its log tail is worth showing.
fn failed(lease: &Lease) -> bool {
    use crate::lease::State;
    matches!(
        lease.state,
        State::CrashLoop { .. } | State::Exited { .. } | State::Unhealthy
    )
}

/// Container names go into a shell command, so only plain names are used.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Hands text to the terminal's clipboard with OSC 52, which works over SSH
/// and needs no clipboard program.
fn copy_to_clipboard(terminal: &mut DefaultTerminal, text: &str) -> Result<()> {
    let backend = terminal.backend_mut();
    write!(backend, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    backend.flush()?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn key_hints(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(GAP));
        }
        spans.push(Span::styled(*key, Style::new().fg(ACCENT)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, dim()));
    }
    Line::from(spans)
}

/// How wide `key_hints` renders `pairs`.
fn hints_width(pairs: &[(&str, &str)]) -> usize {
    pairs
        .iter()
        .map(|(k, l)| view::width(k) + 1 + view::width(l))
        .sum::<usize>()
        + GAP.len() * pairs.len().saturating_sub(1)
}

fn title(text: String) -> Span<'static> {
    Span::styled(text, Style::new().add_modifier(Modifier::BOLD))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"crumb"), "Y3J1bWI=");
        assert_eq!(base64(b"ssh indigo"), "c3NoIGluZGlnbw==");
    }

    #[test]
    fn only_plain_container_names_reach_the_shell() {
        assert!(safe_name("wt_lym_1119-backend-1"));
        assert!(!safe_name("x; rm -rf /"));
        assert!(!safe_name(""));
    }
}
