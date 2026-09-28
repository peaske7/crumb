//! Lifecycle keys: each runs on a background thread and reports its steps
//! back as messages; `d`, `D` and `R` show their plan first.

use std::sync::mpsc::Sender;

use ratatui::crossterm::event::KeyCode;
use ratatui::text::{Line, Span};

use super::{Msg, dim, key_hints, title, tone};
use crate::ops::Progress;
use crate::ops::reap::Plan;
use crate::view::{self, Tone};

/// What an action is doing, shown in place of the lease's status.
pub(super) struct Activity {
    pub verb: &'static str,
    pub last: String,
    pub lines: Vec<String>,
    pub done: bool,
    pub error: Option<String>,
}

impl Activity {
    pub fn new(verb: &'static str) -> Self {
        Self {
            verb,
            last: String::new(),
            lines: Vec::new(),
            done: false,
            error: None,
        }
    }

    pub fn push(&mut self, line: String, step: bool) {
        if step {
            self.last = line.clone();
        }
        self.lines.push(line);
        if self.lines.len() > 200 {
            self.lines.remove(0);
        }
    }

    /// The status phrase while running, or why it failed.
    pub fn status(&self) -> (String, Tone) {
        match &self.error {
            Some(_) => (format!("{} failed", self.verb), Tone::Bad),
            None if self.last.is_empty() => (format!("{}…", self.verb), Tone::Dim),
            None => (format!("{} · {}", self.verb, self.last), Tone::Dim),
        }
    }
}

/// Sends an action's steps to the UI thread.
pub(super) struct TuiProgress {
    pub lease: String,
    pub tx: Sender<Msg>,
}

impl Progress for TuiProgress {
    fn step(&self, verb: &str, text: &str) {
        let _ = self.tx.send(Msg::Progress {
            lease: self.lease.clone(),
            line: format!("{} {text}", verb.to_lowercase()),
            step: true,
        });
    }

    fn detail(&self, text: &str) {
        let _ = self.tx.send(Msg::Progress {
            lease: self.lease.clone(),
            line: text.to_string(),
            step: false,
        });
    }
}

/// A change waiting for confirmation, shown in place of the list.
pub(super) enum Pending {
    Down {
        lease: String,
        lines: Vec<String>,
    },
    Drop {
        lease: String,
        lines: Vec<String>,
        typed: String,
    },
    Reap {
        plan: Plan,
    },
}

pub(super) enum Answer {
    Apply,
    Cancel,
    Wait,
}

impl Pending {
    pub fn key(&mut self, code: KeyCode) -> Answer {
        match (self, code) {
            (_, KeyCode::Esc) => Answer::Cancel,
            (Pending::Drop { lease, typed, .. }, code) => match code {
                KeyCode::Enter if typed == lease => Answer::Apply,
                KeyCode::Enter => Answer::Wait,
                KeyCode::Backspace => {
                    typed.pop();
                    Answer::Wait
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    Answer::Wait
                }
                _ => Answer::Wait,
            },
            (_, KeyCode::Char('y') | KeyCode::Enter) => Answer::Apply,
            (_, KeyCode::Char('q') | KeyCode::Char('n')) => Answer::Cancel,
            _ => Answer::Wait,
        }
    }

    pub fn header(&self, host: &str) -> Line<'static> {
        let text = match self {
            Pending::Down { lease, .. } => format!("down {lease}"),
            Pending::Drop { lease, .. } => format!("drop {lease}"),
            Pending::Reap { .. } => "reap orphans".to_string(),
        };
        Line::from(vec![
            title("crumb".to_string()),
            Span::raw("  "),
            Span::styled(format!("{text} · {host}"), dim()),
        ])
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        match self {
            Pending::Down { lines, .. } | Pending::Drop { lines, .. } => {
                for line in lines {
                    out.push(Line::from(vec![Span::raw("  "), Span::raw(line.clone())]));
                }
            }
            Pending::Reap { plan } => {
                let rows = plan.rows();
                let width = rows
                    .iter()
                    .map(|(_, l, _)| view::width(l))
                    .max()
                    .unwrap_or(0);
                for (verb, lease, detail) in rows {
                    let t = match verb {
                        "stop" | "down" => Tone::Warn,
                        _ => Tone::Dim,
                    };
                    out.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(view::pad(verb, 6), tone(t)),
                        Span::raw(view::pad(&lease, width + 3)),
                        Span::styled(detail, dim()),
                    ]));
                }
                if plan.changes() == 0 {
                    out.push(Line::default());
                    out.push(Line::styled("  nothing to change yet", dim()));
                }
            }
        }
        if let Pending::Drop { lease, typed, .. } = self {
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw("  type "),
                Span::styled(lease.clone(), tone(Tone::Bad)),
                Span::raw(" to drop its database: "),
                Span::raw(typed.clone()),
                Span::styled("▏", dim()),
            ]));
        }
        out
    }

    pub fn keys(&self) -> Line<'static> {
        match self {
            Pending::Drop { .. } => key_hints(&[("⏎", "drop"), ("esc", "cancel")]),
            Pending::Reap { plan } if plan.changes() == 0 => key_hints(&[("esc", "back")]),
            _ => key_hints(&[("y", "apply"), ("esc", "cancel")]),
        }
    }
}
