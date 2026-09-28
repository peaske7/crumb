use std::io::{IsTerminal, Write};

use anyhow::Result;
use jiff::Timestamp;

use crate::lease::{Group, Lease};
use crate::snapshot::Snapshot;
use crate::view::{self, Tone};

const GAP: &str = "   ";

pub fn print(snapshot: &Snapshot, json: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if json {
        serde_json::to_writer_pretty(&mut out, &snapshot.leases)?;
        writeln!(out)?;
        return Ok(());
    }
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    out.write_all(render(&snapshot.leases, Timestamp::now(), color).as_bytes())?;
    for warning in &snapshot.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

pub fn render(leases: &[Lease], now: Timestamp, color: bool) -> String {
    let paint = |text: &str, tone: Tone| paint(text, tone, color);
    let rows: Vec<(&Lease, view::Row)> = leases
        .iter()
        .filter(|l| l.group != Group::DatabaseOnly)
        .map(|l| (l, view::row(l, now)))
        .collect();
    let widths = view::widths(rows.iter().map(|(_, row)| row));

    let mut groups: Vec<Group> = leases.iter().map(|l| l.group).collect();
    groups.dedup();
    let headed = groups.len() > 1;

    let mut out = String::new();
    for group in groups {
        let members: Vec<&Lease> = leases.iter().filter(|l| l.group == group).collect();
        if headed {
            let heading = match group {
                Group::Orphaned => {
                    let total = view::total_memory(members.iter().copied())
                        .map(|bytes| paint(&format!(" · {}", view::memory(bytes)), Tone::Dim))
                        .unwrap_or_default();
                    format!("{}{total}", paint("orphaned", Tone::Warn))
                }
                _ => paint(view::group_title(group), Tone::Dim),
            };
            out.push_str(&heading);
            out.push('\n');
        }
        if group == Group::DatabaseOnly {
            let names: Vec<&str> = members
                .iter()
                .map(|l| {
                    l.database
                        .as_ref()
                        .map_or(l.name.as_str(), |d| d.name.as_str())
                })
                .collect();
            out.push_str(&format!("  {}\n", paint(&names.join(GAP), Tone::Dim)));
            continue;
        }
        for (_, row) in rows.iter().filter(|(l, _)| l.group == group) {
            let quiet = |tone: Tone| if row.quiet { Tone::Dim } else { tone };
            let status_tone = match row.status.1 {
                Tone::Bad => Tone::Bad,
                tone => quiet(tone),
            };
            let line = [
                paint(&view::pad(&row.name, widths.name), quiet(Tone::Normal)),
                paint(
                    &view::pad(&row.worktree.0, widths.worktree),
                    quiet(row.worktree.1),
                ),
                paint(&view::pad(&row.port, widths.port), quiet(Tone::Normal)),
                paint(&view::pad(&row.status.0, widths.status), status_tone),
                paint(&row.memory, Tone::Dim),
            ]
            .join(GAP);
            out.push_str(&format!("  {}\n", line.trim_end()));
        }
    }
    out
}

fn paint(text: &str, tone: Tone, color: bool) -> String {
    let code = match tone {
        _ if !color => return text.to_string(),
        Tone::Normal => return text.to_string(),
        Tone::Dim => "2",
        Tone::Warn => "33",
        Tone::Bad => "31",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{Database, State, Worktree};

    fn lease(name: &str, group: Group, state: State) -> Lease {
        Lease {
            name: name.into(),
            group,
            state,
            reasons: vec![],
            worktree: Some(Worktree {
                path: format!("/work/{name}"),
                exists: group != Group::Orphaned,
            }),
            port: Some(8101),
            memory_bytes: Some(300 * 1024 * 1024),
            restarts: 0,
            since: Some("2026-09-28T11:00:00Z".parse().unwrap()),
            sync: None,
            database: Some(Database {
                name: format!("wt_{name}"),
                comment: None,
            }),
        }
    }

    #[test]
    fn plain_text_groups_rows_and_lists_bare_databases() {
        let now: Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
        let leases = [
            lease("a", Group::NeedsYou, State::Exited { code: 1 }),
            lease("b", Group::Orphaned, State::Healthy),
            lease("c", Group::DatabaseOnly, State::Absent),
        ];
        let text = render(&leases, now, false);
        assert_eq!(
            text,
            "needs you\n\
             \x20 a   a               :8101   exited 1 · 1h ago   300M\n\
             orphaned · 300M\n\
             \x20 b   worktree gone   :8101   up 1h               300M\n\
             databases only\n\
             \x20 wt_c\n"
        );
    }

    #[test]
    fn a_single_group_has_no_heading() {
        let now: Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
        let text = render(&[lease("a", Group::Running, State::Healthy)], now, false);
        assert!(text.starts_with("  a "));
    }
}
