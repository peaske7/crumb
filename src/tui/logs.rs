//! A lease's backend logs, followed live. Leaving the view stops the stream.

use std::collections::VecDeque;

use anyhow::Result;
use ratatui::crossterm::event::KeyCode;
use ratatui::text::{Line, Span};

use super::{dim, key_hints, title, tone};
use crate::run::Stream;
use crate::view::{self, Tone};

/// Lines kept in memory; older ones scroll away.
const KEEP: usize = 5_000;

pub(super) struct Logs {
    pub lease: String,
    lines: VecDeque<String>,
    /// Pinned to the newest line.
    follow: bool,
    /// First visible line while not following.
    top: usize,
    _stream: Option<Stream>,
    error: Option<String>,
}

impl Logs {
    pub fn new(lease: String, stream: Result<Stream>) -> Self {
        let (stream, error) = match stream {
            Ok(stream) => (Some(stream), None),
            Err(err) => (None, Some(format!("{err:#}"))),
        };
        Self {
            lease,
            lines: VecDeque::new(),
            follow: true,
            top: 0,
            _stream: stream,
            error,
        }
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() == KEEP {
            self.lines.pop_front();
            self.top = self.top.saturating_sub(1);
        }
        self.lines.push_back(view::clean_log_line(&line));
    }

    fn last_top(&self, height: usize) -> usize {
        self.lines.len().saturating_sub(height)
    }

    fn visible_top(&self, height: usize) -> usize {
        if self.follow {
            self.last_top(height)
        } else {
            self.top.min(self.last_top(height))
        }
    }

    pub fn key(&mut self, code: KeyCode, height: usize) {
        let top = self.visible_top(height);
        let page = height.max(1);
        match code {
            KeyCode::Char('k') | KeyCode::Up => self.scroll(top.saturating_sub(1), height),
            KeyCode::Char('j') | KeyCode::Down => self.scroll(top + 1, height),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll(top.saturating_sub(page), height),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll(top + page, height),
            KeyCode::Char('g') | KeyCode::Home => self.scroll(0, height),
            KeyCode::Char('G') | KeyCode::End => self.follow = true,
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                self.top = top;
            }
            _ => {}
        }
    }

    /// Scrolling back to the end resumes following.
    fn scroll(&mut self, top: usize, height: usize) {
        if top >= self.last_top(height) {
            self.follow = true;
        } else {
            self.follow = false;
            self.top = top;
        }
    }

    pub fn header(&self, status: Option<(String, Tone)>) -> Line<'static> {
        let mut spans = vec![title(self.lease.clone()), Span::styled(" · logs", dim())];
        if let Some((text, t)) = status {
            spans.push(Span::styled(" · ", dim()));
            spans.push(Span::styled(text, tone(t)));
        }
        if let Some(error) = &self.error {
            spans.push(Span::styled(format!(" · {error}"), tone(Tone::Bad)));
        }
        Line::from(spans)
    }

    pub fn lines(&self, height: usize) -> Vec<Line<'static>> {
        if self.lines.is_empty() && self.error.is_none() {
            return vec![Line::styled("waiting for output…", dim())];
        }
        self.lines
            .iter()
            .skip(self.visible_top(height))
            .take(height)
            .map(|line| {
                let t = if view::is_error_line(line) {
                    Tone::Bad
                } else {
                    Tone::Dim
                };
                Line::styled(line.clone(), tone(t))
            })
            .collect()
    }

    pub fn keys(&self) -> Line<'static> {
        let follow = if self.follow { "following" } else { "follow" };
        key_hints(&[
            ("↑↓", "scroll"),
            ("f", follow),
            ("g", "top"),
            ("esc", "back"),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs(count: usize) -> Logs {
        let mut logs = Logs::new("a".into(), Err(anyhow::anyhow!("no stream")));
        for i in 0..count {
            logs.push(format!("line {i}"));
        }
        logs
    }

    fn first(logs: &Logs, height: usize) -> String {
        logs.lines(height)[0].spans[0].content.to_string()
    }

    #[test]
    fn follows_the_newest_lines() {
        let logs = logs(100);
        assert_eq!(first(&logs, 10), "line 90");
    }

    #[test]
    fn scrolling_up_stops_following_and_the_end_resumes_it() {
        let mut logs = logs(100);
        logs.key(KeyCode::Up, 10);
        assert_eq!(first(&logs, 10), "line 89");
        logs.push("line 100".into());
        assert_eq!(
            first(&logs, 10),
            "line 89",
            "held still while new lines arrive"
        );
        logs.key(KeyCode::End, 10);
        assert_eq!(first(&logs, 10), "line 91");
    }

    #[test]
    fn top_and_paging() {
        let mut logs = logs(100);
        logs.key(KeyCode::Char('g'), 10);
        assert_eq!(first(&logs, 10), "line 0");
        logs.key(KeyCode::PageDown, 10);
        assert_eq!(first(&logs, 10), "line 10");
    }
}
