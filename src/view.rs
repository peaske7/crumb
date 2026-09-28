//! Words and tones shared by the TUI and `crumb ls`, so both say the same thing.

use jiff::Timestamp;

use crate::lease::{Group, Lease, Reason, State};
use crate::run::Record;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Dim,
    Warn,
    Bad,
}

pub fn group_title(group: Group) -> &'static str {
    match group {
        Group::NeedsYou => "needs you",
        Group::Running => "running",
        Group::Orphaned => "orphaned",
        Group::DatabaseOnly => "databases only",
    }
}

/// The one status phrase for a lease row.
pub fn status(lease: &Lease, now: Timestamp) -> (String, Tone) {
    // "healthy · 3h", "exited 1 · 22m ago": the word, then how long.
    let with_age = |word: String, suffix: &str| match lease.since {
        Some(since) => format!("{word} · {}{suffix}", age(since, now)),
        None => word,
    };
    let up = || match lease.since {
        Some(since) => format!("up {}", age(since, now)),
        None => "up".to_string(),
    };
    // A running lease that is behind says so instead of how long it has run,
    // naming the thing most likely to bite first.
    if lease.group == Group::Running
        && let Some(reason) = BEHIND.iter().find(|r| lease.reasons.contains(r))
    {
        let text = match reason {
            Reason::WiringRemote => remote_wiring(lease)
                .map(|(_, url)| format!("wired to {}", url_host(&url)))
                .unwrap_or_else(|| "wired remote".to_string()),
            reason => behind_word(reason).to_string(),
        };
        return (text, Tone::Warn);
    }
    match &lease.state {
        State::Healthy if lease.group == Group::Orphaned => (up(), Tone::Dim),
        State::Healthy => (with_age("healthy".into(), ""), Tone::Normal),
        State::Running => (up(), Tone::Normal),
        State::Starting => (with_age("starting".into(), ""), Tone::Dim),
        State::Unhealthy => (with_age("unhealthy".into(), ""), Tone::Bad),
        State::CrashLoop { restarts } => {
            (format!("crash loop · {}", thousands(*restarts)), Tone::Bad)
        }
        State::Exited { code } => (with_age(format!("exited {code}"), " ago"), Tone::Bad),
        State::Stopped => (with_age("stopped".into(), " ago"), Tone::Dim),
        State::Absent if lease.sync.is_none() => ("not started".to_string(), Tone::Dim),
        State::Absent => ("no containers".to_string(), Tone::Warn),
    }
}

/// Behind reasons, most urgent first.
const BEHIND: [Reason; 6] = [
    Reason::DepsBehind,
    Reason::SchemaBehind,
    Reason::WiringRemote,
    Reason::NoForward,
    Reason::WiringStale,
    Reason::RestartPending,
];

fn behind_word(reason: &Reason) -> &'static str {
    match reason {
        Reason::DepsBehind => "deps behind",
        Reason::SchemaBehind => "schema behind",
        Reason::NoForward => "no forward",
        Reason::WiringStale => "wiring stale",
        _ => "restart pending",
    }
}

fn remote_wiring(lease: &Lease) -> Option<(String, String)> {
    lease.wiring.iter().find_map(|w| match &w.status {
        crate::wire::Status::Remote { url } => Some((w.file.clone(), url.clone())),
        _ => None,
    })
}

/// `http://100.64.0.1:8107/x` → `100.64.0.1`.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

pub fn worktree(lease: &Lease) -> (String, Tone) {
    match &lease.worktree {
        Some(w) if !w.exists => ("worktree gone".to_string(), Tone::Dim),
        Some(w) => (
            w.path.rsplit('/').next().unwrap_or(&w.path).to_string(),
            Tone::Normal,
        ),
        None => ("—".to_string(), Tone::Dim),
    }
}

/// The port to use from this machine: the forward's when there is one.
pub fn port(lease: &Lease) -> String {
    lease
        .local_port
        .or(lease.port)
        .map(|p| format!(":{p}"))
        .unwrap_or_default()
}

pub fn memory(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= 1024 * MIB {
        format!("{:.1}G", bytes as f64 / (1024 * MIB) as f64)
    } else {
        format!("{}M", bytes / MIB)
    }
}

/// Explanations for the expanded row: `(label, text, tone)`.
pub fn reasons(lease: &Lease, now: Timestamp) -> Vec<(&'static str, String, Tone)> {
    lease
        .reasons
        .iter()
        .map(|reason| match reason {
            Reason::WorktreeGone => (
                "worktree",
                format!(
                    "{} no longer exists",
                    lease.worktree.as_ref().map_or("?", |w| w.path.as_str())
                ),
                Tone::Normal,
            ),
            Reason::CrashLoop => (
                "backend",
                format!(
                    "restarted {} times without becoming healthy",
                    thousands(lease.restarts)
                ),
                Tone::Bad,
            ),
            Reason::Exited => {
                let code = match lease.state {
                    State::Exited { code } => code,
                    _ => 0,
                };
                let when = lease
                    .since
                    .map(|since| format!(" {} ago", age(since, now)))
                    .unwrap_or_default();
                (
                    "backend",
                    format!("exited with code {code}{when}"),
                    Tone::Bad,
                )
            }
            Reason::Unhealthy => (
                "backend",
                "running but failing its health check".to_string(),
                Tone::Bad,
            ),
            Reason::NoContainers => (
                "backend",
                "no containers; the sync and database are still there".to_string(),
                Tone::Warn,
            ),
            Reason::SyncPaused => ("sync", "paused".to_string(), Tone::Warn),
            Reason::SyncDisconnected => ("sync", "not connected".to_string(), Tone::Warn),
            Reason::SyncConflicts => (
                "sync",
                format!(
                    "{} conflicts",
                    lease.sync.as_ref().map_or(0, |s| s.conflicts)
                ),
                Tone::Warn,
            ),
            Reason::SyncError => (
                "sync",
                lease
                    .sync
                    .as_ref()
                    .and_then(|s| s.error.clone())
                    .unwrap_or_default(),
                Tone::Bad,
            ),
            Reason::RestartPending => (
                "build",
                format!(
                    "{} changed after the backend started",
                    lease.changed_file.as_deref().unwrap_or("a file")
                ),
                Tone::Warn,
            ),
            Reason::DepsBehind => {
                let built = lease
                    .image_built
                    .map(|at| format!(", built {} ago", age(at, now)))
                    .unwrap_or_default();
                (
                    "deps",
                    format!(
                        "the lockfile differs from the image's{built}; the next start may fail"
                    ),
                    Tone::Warn,
                )
            }
            Reason::SchemaBehind => (
                "schema",
                format!(
                    "migrations reach {}; the database has {}. m migrates",
                    lease.newest_migration.as_deref().unwrap_or("?"),
                    lease
                        .database
                        .as_ref()
                        .and_then(|d| d.applied.as_deref())
                        .unwrap_or("?"),
                ),
                Tone::Warn,
            ),
            Reason::NoForward => {
                let port = lease.port.map(|p| format!(":{p}")).unwrap_or_default();
                let text = match &lease.forward {
                    None => format!("nothing forwards {port} to this machine; t creates it"),
                    Some(f) if f.paused => "the forward is paused; t resumes it".to_string(),
                    Some(f) if f.remote_port != lease.port => format!(
                        "the forward points at :{}, the backend is on {port}; t fixes it",
                        f.remote_port.unwrap_or_default()
                    ),
                    Some(f) => format!(
                        "the forward is not connected{}; t recreates it",
                        f.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
                    ),
                };
                ("port", text, Tone::Warn)
            }
            Reason::WiringStale => {
                let files: Vec<&str> = lease
                    .wiring
                    .iter()
                    .filter(|w| w.status != crate::wire::Status::Ok)
                    .map(|w| w.file.as_str())
                    .collect();
                (
                    "wiring",
                    format!("{} missing or out of date; t rewrites", files.join(", ")),
                    Tone::Warn,
                )
            }
            Reason::WiringRemote => {
                let (file, url) = remote_wiring(lease).unwrap_or_default();
                (
                    "wiring",
                    format!(
                        "{file} points at {url}; WebSocket clients upgrade non-loopback hosts to wss:, so live connections fail. t rewires through a forward"
                    ),
                    Tone::Warn,
                )
            }
        })
        .collect()
}

pub fn age(since: Timestamp, now: Timestamp) -> String {
    let secs = now.duration_since(since).as_secs().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The cells of one lease row, before styling.
pub struct Row {
    pub name: String,
    pub worktree: (String, Tone),
    pub port: String,
    pub status: (String, Tone),
    pub memory: String,
    /// Orphans are no longer anyone's work, so the whole row is quiet.
    pub quiet: bool,
}

pub fn row(lease: &Lease, now: Timestamp) -> Row {
    Row {
        name: lease.name.clone(),
        worktree: worktree(lease),
        port: port(lease),
        status: status(lease, now),
        memory: lease.memory_bytes.map(memory).unwrap_or_default(),
        quiet: lease.group == Group::Orphaned,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Widths {
    pub name: usize,
    pub worktree: usize,
    pub port: usize,
    pub status: usize,
}

pub fn widths<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Widths {
    rows.into_iter().fold(Widths::default(), |w, r| Widths {
        name: w.name.max(width(&r.name)),
        worktree: w.worktree.max(width(&r.worktree.0)),
        port: w.port.max(width(&r.port)),
        status: w.status.max(width(&r.status.0)),
    })
}

pub fn width(text: &str) -> usize {
    text.chars().count()
}

pub fn pad(text: &str, to: usize) -> String {
    format!("{text}{}", " ".repeat(to.saturating_sub(width(text))))
}

/// Total memory of a set of leases, for the orphaned heading.
pub fn total_memory<'a>(leases: impl IntoIterator<Item = &'a Lease>) -> Option<u64> {
    let total: u64 = leases.into_iter().filter_map(|l| l.memory_bytes).sum();
    (total > 0).then_some(total)
}

/// Word-wraps `text` to `width` columns; a word longer than a line is split.
pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(10);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        while width(&word) > columns {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            lines.push(word.chars().take(columns).collect());
            word = word.chars().skip(columns).collect();
        }
        if !line.is_empty() && width(&line) + 1 + width(&word) > columns {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Whether a log line reports a failure.
pub fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["error", "fatal", "panic", "exception", "err_"]
        .iter()
        .any(|word| lower.contains(word))
}

/// Whether a line opens an error report: `Error: …`, `TypeError [X]: …`,
/// `error[E0308]: …`, `panic: …`, `thread 'main' panicked at …`.
pub fn is_error_headline(line: &str) -> bool {
    let line = line.trim_start();
    let word: String = line
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let rest = &line[word.len()..];
    let named = (word.ends_with("Error") || word.ends_with("Exception"))
        && (rest.starts_with(':') || rest.starts_with(" [") || rest.starts_with(" ("));
    let prefixed = ["error:", "error[", "fatal:", "FATAL", "panic:"]
        .iter()
        .any(|prefix| line.starts_with(prefix));
    named || prefixed || line.contains("panicked at")
}

/// The lines worth showing under a failed lease: the last distinct error
/// headlines, else the last lines that mention an error, else the last lines.
pub fn excerpt(lines: &[String], limit: usize) -> Vec<String> {
    let pick = |matches: &dyn Fn(&str) -> bool| -> Vec<String> {
        let mut picked: Vec<String> = Vec::new();
        for line in lines.iter().rev().map(|l| l.trim()) {
            if picked.len() == limit {
                break;
            }
            if !line.is_empty() && matches(line) && !picked.iter().any(|p| p == line) {
                picked.push(line.to_string());
            }
        }
        picked.reverse();
        picked
    };
    let headlines = pick(&is_error_headline);
    if !headlines.is_empty() {
        return headlines;
    }
    let errors = pick(&is_error_line);
    if !errors.is_empty() {
        return errors;
    }
    pick(&|_| true)
}

/// Removes terminal escape sequences and tabs so log lines render cleanly.
pub fn clean_log_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => {
                // CSI: ESC [ params final-byte. Anything else: drop the ESC.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\t' => out.push_str("    "),
            '\r' => {}
            ch => out.push(ch),
        }
    }
    out
}

/// One row of the command log: the latest run of a command and how many
/// times it ran. Refresh commands would otherwise fill the log.
pub struct Folded {
    pub record: Record,
    pub count: usize,
}

pub fn fold(records: &[Record]) -> Vec<Folded> {
    let mut folded: Vec<Folded> = Vec::new();
    for record in records {
        let same = |f: &Folded| f.record.display == record.display && f.record.ok() == record.ok();
        match folded.iter().position(same) {
            Some(index) => {
                let mut entry = folded.remove(index);
                entry.record = record.clone();
                entry.count += 1;
                folded.push(entry);
            }
            None => folded.push(Folded {
                record: record.clone(),
                count: 1,
            }),
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_like_a_person_would_say_them() {
        let now: Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
        let ago = |s: &str| age(s.parse().unwrap(), now);
        assert_eq!(ago("2026-09-28T11:59:48Z"), "12s");
        assert_eq!(ago("2026-09-28T11:38:00Z"), "22m");
        assert_eq!(ago("2026-09-28T07:00:00Z"), "5h");
        assert_eq!(ago("2026-09-23T12:00:00Z"), "5d");
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(
            wrap("points at http://100.64.0.1:8107; live sockets fail", 20),
            [
                "points at",
                "http://100.64.0.1:81",
                "07; live sockets",
                "fail"
            ]
        );
        assert_eq!(wrap("", 20), [""]);
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(25_943), "25,943");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn memory_units() {
        assert_eq!(memory(396 * 1024 * 1024), "396M");
        assert_eq!(memory(1_700_000_000), "1.6G");
    }

    #[test]
    fn excerpt_prefers_error_headlines() {
        // The tail of a real crash loop: a thrown error, Node's source excerpt
        // and property dump, then the wrapper's own error.
        let lines: Vec<String> = [
            "Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'x' imported from /app/dist/index.js",
            "  code: 'ERR_MODULE_NOT_FOUND'",
            "Node.js v24",
            "        new Error(`${command} exited with code ${code}`),",
            "Error: pnpm start exited with code 1",
            "[docker-dev] building backend dist (tsdown)…",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            excerpt(&lines, 3),
            [
                "Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'x' imported from /app/dist/index.js",
                "Error: pnpm start exited with code 1",
            ]
        );
    }

    #[test]
    fn excerpt_falls_back_to_error_words_then_the_tail() {
        let words: Vec<String> = ["ok", "db connection error", "ok"]
            .map(String::from)
            .to_vec();
        assert_eq!(excerpt(&words, 3), ["db connection error"]);
        let quiet: Vec<String> = ["a", "", "b", "c"].map(String::from).to_vec();
        assert_eq!(excerpt(&quiet, 2), ["b", "c"]);
    }

    #[test]
    fn headlines_across_languages() {
        assert!(is_error_headline("TypeError: x is not a function"));
        assert!(is_error_headline("error[E0308]: mismatched types"));
        assert!(is_error_headline(
            "thread 'main' panicked at src/main.rs:3:5"
        ));
        assert!(is_error_headline("ValueError: bad input"));
        assert!(!is_error_headline("  code: 'ERR_MODULE_NOT_FOUND'"));
        assert!(!is_error_headline("new Error(`boom`)"));
    }

    #[test]
    fn log_lines_lose_escape_codes() {
        assert_eq!(
            clean_log_line("\u{1b}[32minfo\u{1b}[0m\tready\r"),
            "info    ready"
        );
    }

    #[test]
    fn repeated_commands_fold_into_their_latest_run() {
        use crate::run::Outcome;
        use std::time::Duration;
        let record = |display: &str, outcome| Record {
            at: "2026-09-28T12:00:00Z".parse().unwrap(),
            display: display.into(),
            duration: Duration::from_millis(50),
            outcome,
        };
        let records = [
            record("probe", Outcome::Exited(0)),
            record("mutagen", Outcome::Exited(0)),
            record("probe", Outcome::Exited(0)),
            record("probe", Outcome::Exited(1)),
            record("mutagen", Outcome::Exited(0)),
        ];
        let folded: Vec<(String, usize, bool)> = fold(&records)
            .into_iter()
            .map(|f| (f.record.display.clone(), f.count, f.record.ok()))
            .collect();
        assert_eq!(
            folded,
            [
                ("probe".to_string(), 2, true),
                ("probe".to_string(), 1, false),
                ("mutagen".to_string(), 2, true),
            ]
        );
    }
}
