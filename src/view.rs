//! Words and tones shared by the TUI and `crumb ls`, so both say the same thing.

use jiff::Timestamp;

use crate::lease::{Group, Lease, Reason, State};

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
        State::Absent => ("no containers".to_string(), Tone::Warn),
    }
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

pub fn port(lease: &Lease) -> String {
    lease.port.map(|p| format!(":{p}")).unwrap_or_default()
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
}
