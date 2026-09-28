//! Reap: orphaned leases are stopped at once and brought down after seven
//! days stopped. The clock is Docker's own `FinishedAt`. Databases are never
//! touched.

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};

use super::Ctx;
use super::verbs::{down_lease, stop_lease};
use crate::lease::{Group, Lease, State};
use crate::snapshot::Snapshot;
use crate::view;

pub const GRACE: SignedDuration = SignedDuration::from_hours(7 * 24);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Stop,
    Down,
    /// Stopped, and not yet for seven days.
    Keep,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub lease: Lease,
    pub action: Action,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Databases of the orphans, which reap keeps.
    pub databases: Vec<String>,
}

impl Plan {
    pub fn changes(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.action != Action::Keep)
            .count()
    }

    /// `(verb, lease, detail)` rows for display.
    pub fn rows(&self) -> Vec<(&'static str, String, String)> {
        let mut rows: Vec<_> = self
            .items
            .iter()
            .map(|item| {
                let verb = match item.action {
                    Action::Stop => "stop",
                    Action::Down => "down",
                    Action::Keep => "keep",
                };
                (verb, item.lease.name.clone(), item.detail.clone())
            })
            .collect();
        if !self.databases.is_empty() {
            rows.push((
                "keep",
                "databases".to_string(),
                format!("{} (crumb drop removes one)", self.databases.join(", ")),
            ));
        }
        rows
    }
}

pub fn plan(snapshot: &Snapshot, now: Timestamp) -> Plan {
    let mut plan = Plan::default();
    for lease in snapshot
        .leases
        .iter()
        .filter(|l| l.group == Group::Orphaned)
    {
        let running = lease.is_running()
            || matches!(lease.state, State::CrashLoop { .. })
            || lease.memory_bytes.is_some();
        let (action, detail) = if running {
            let mut parts = vec![match lease.memory_bytes {
                Some(bytes) => format!("stop its containers ({})", view::memory(bytes)),
                None => "stop its containers".to_string(),
            }];
            if lease.sync.as_ref().is_some_and(|s| !s.paused) {
                parts.push("pause its sync".to_string());
            }
            if lease.forward.as_ref().is_some_and(|f| !f.paused) {
                parts.push("pause its forward".to_string());
            }
            (Action::Stop, parts.join(", "))
        } else {
            match lease.since {
                Some(since) if now.duration_since(since) < GRACE => {
                    let left = GRACE - now.duration_since(since);
                    (
                        Action::Keep,
                        format!(
                            "stopped {} ago; down in {}",
                            view::age(since, now),
                            view::age(now, now + left)
                        ),
                    )
                }
                since => (
                    Action::Down,
                    format!(
                        "{}remove its containers, sync and replica",
                        since
                            .map(|s| format!("stopped {} ago: ", view::age(s, now)))
                            .unwrap_or_default()
                    ),
                ),
            }
        };
        if let Some(db) = &lease.database {
            plan.databases.push(db.name.clone());
        }
        plan.items.push(Item {
            lease: lease.clone(),
            action,
            detail,
        });
    }
    plan
}

/// Applies the plan, carrying on past a lease that fails. Returns the
/// failures as `(lease, error)`.
pub fn apply(ctx: &Ctx, plan: &Plan) -> Vec<(String, String)> {
    let mut failures = Vec::new();
    for item in &plan.items {
        let result: Result<()> = match item.action {
            Action::Stop => stop_lease(ctx, &item.lease),
            Action::Down => down_lease(ctx, &item.lease),
            Action::Keep => Ok(()),
        };
        if let Err(err) = result {
            ctx.out
                .step("Failed", &format!("{}: {err:#}", item.lease.name));
            failures.push((item.lease.name.clone(), format!("{err:#}")));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{Database, Sync, Worktree};

    fn orphan(name: &str, state: State, since: &str) -> Lease {
        Lease {
            group: Group::Orphaned,
            state,
            since: Some(since.parse().unwrap()),
            worktree: Some(Worktree {
                path: format!("/gone/{name}"),
                exists: false,
            }),
            sync: Some(Sync {
                session: format!("wt-{name}"),
                status: "watching".into(),
                paused: false,
                connected: false,
                conflicts: 0,
                error: None,
            }),
            database: Some(Database {
                name: format!("wt_{name}"),
                comment: None,
                applied: None,
            }),
            ..Lease::new(name)
        }
    }

    #[test]
    fn stops_running_orphans_and_downs_old_stopped_ones() {
        let now: Timestamp = "2026-09-28T12:00:00Z".parse().unwrap();
        let snapshot = Snapshot {
            at: now,
            host: "box".into(),
            mem: None,
            leases: vec![
                orphan("a", State::Healthy, "2026-09-26T12:00:00Z"),
                orphan(
                    "b",
                    State::CrashLoop { restarts: 9 },
                    "2026-09-28T11:00:00Z",
                ),
                orphan("c", State::Stopped, "2026-09-26T12:00:00Z"),
                orphan("d", State::Exited { code: 1 }, "2026-09-18T12:00:00Z"),
                Lease {
                    group: Group::Running,
                    ..Lease::new("live")
                },
            ],
            warnings: vec![],
        };
        let plan = plan(&snapshot, now);
        let actions: Vec<(&str, Action)> = plan
            .items
            .iter()
            .map(|i| (i.lease.name.as_str(), i.action.clone()))
            .collect();
        assert_eq!(
            actions,
            [
                ("a", Action::Stop),
                ("b", Action::Stop),
                ("c", Action::Keep),
                ("d", Action::Down),
            ]
        );
        assert_eq!(plan.items[0].detail, "stop its containers, pause its sync");
        assert_eq!(plan.items[2].detail, "stopped 2d ago; down in 5d");
        assert_eq!(plan.changes(), 3);
        assert_eq!(plan.databases, ["wt_a", "wt_b", "wt_c", "wt_d"]);
    }
}
