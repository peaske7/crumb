//! Every command crumb ran this session, newest last, with `y` to copy one.

use ratatui::crossterm::event::KeyCode;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{ACCENT, dim, key_hints, title, tone};
use crate::view::{Folded, Tone};

#[derive(Default)]
pub(super) struct Commands {
    /// The selected command and whether it succeeded; none means the newest.
    selected: Option<(String, bool)>,
}

impl Commands {
    fn index(&self, rows: &[Folded]) -> usize {
        self.selected
            .as_ref()
            .and_then(|(display, ok)| {
                rows.iter()
                    .position(|r| &r.record.display == display && r.record.ok() == *ok)
            })
            .unwrap_or(rows.len().saturating_sub(1))
    }

    fn select(&mut self, rows: &[Folded], index: usize) {
        self.selected = rows
            .get(index.min(rows.len().saturating_sub(1)))
            .map(|r| (r.record.display.clone(), r.record.ok()));
    }

    /// Returns the command to copy when `y` is pressed.
    pub fn key(&mut self, code: KeyCode, rows: &[Folded]) -> Option<String> {
        let index = self.index(rows);
        match code {
            KeyCode::Char('k') | KeyCode::Up => self.select(rows, index.saturating_sub(1)),
            KeyCode::Char('j') | KeyCode::Down => self.select(rows, index + 1),
            KeyCode::Char('g') | KeyCode::Home => self.select(rows, 0),
            KeyCode::Char('G') | KeyCode::End => self.selected = None,
            KeyCode::Char('y') => return rows.get(index).map(|r| r.record.display.clone()),
            _ => {}
        }
        None
    }

    pub fn lines(&self, rows: &[Folded], height: usize) -> Vec<Line<'static>> {
        if rows.is_empty() {
            return vec![Line::styled("no commands yet", dim())];
        }
        let index = self.index(rows);
        let start = (index + 1).saturating_sub(height.max(1));
        rows.iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(i, row)| {
                let record = &row.record;
                let time = record
                    .at
                    .to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%H:%M:%S")
                    .to_string();
                let result_tone = if record.ok() { Tone::Dim } else { Tone::Bad };
                let marker = if i == index {
                    Span::styled("▌ ", Style::new().fg(ACCENT))
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![
                    marker,
                    Span::styled(time, dim()),
                    Span::styled(format!("  {:>8}  ", record.result()), tone(result_tone)),
                    Span::raw(record.display.clone()),
                ];
                if row.count > 1 {
                    spans.push(Span::styled(format!("   ×{}", row.count), dim()));
                }
                Line::from(spans)
            })
            .collect()
    }
}

pub(super) fn header() -> Line<'static> {
    Line::from(title("commands".to_string()))
}

pub(super) fn keys(notice: Option<&'static str>) -> Line<'static> {
    let mut line = key_hints(&[("↑↓", "move"), ("y", "copy"), ("esc", "back")]);
    if let Some(notice) = notice {
        line.spans.push(Span::styled(format!("   {notice}"), dim()));
    }
    line
}
