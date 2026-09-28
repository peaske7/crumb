use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::Serialize;

use crate::config::{Config, Host};
use crate::mutagen::SyncSession;
use crate::probe::{Binding, Container, DatabaseFact, HostFacts};

/// Five failed starts in a row without becoming healthy is a crash loop.
const CRASH_LOOP_RESTARTS: u64 = 5;

/// Where a lease is listed, in the order you act on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    NeedsYou,
    Running,
    Orphaned,
    DatabaseOnly,
}

/// The state of the lease's main container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    Healthy,
    /// Running, and the image defines no health check.
    Running,
    Starting,
    Unhealthy,
    CrashLoop {
        restarts: u64,
    },
    Exited {
        code: i64,
    },
    Stopped,
    /// No containers at all.
    Absent,
}

/// Why a lease needs attention. Serialized for `--json` consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    WorktreeGone,
    CrashLoop,
    Exited,
    Unhealthy,
    NoContainers,
    SyncPaused,
    SyncConflicts,
    SyncError,
    SyncDisconnected,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lease {
    pub name: String,
    pub group: Group,
    pub state: State,
    pub reasons: Vec<Reason>,
    pub worktree: Option<Worktree>,
    /// The main container's name, for logs.
    pub container: Option<String>,
    pub port: Option<u16>,
    pub memory_bytes: Option<u64>,
    pub restarts: u64,
    /// When the main container started (running) or stopped (otherwise).
    pub since: Option<Timestamp>,
    pub sync: Option<Sync>,
    pub database: Option<Database>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub path: String,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sync {
    pub session: String,
    pub status: String,
    pub paused: bool,
    pub connected: bool,
    pub conflicts: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Database {
    pub name: String,
    pub comment: Option<String>,
}

#[derive(Default)]
struct Parts<'a> {
    containers: Vec<&'a Container>,
    sync: Option<&'a SyncSession>,
    database: Option<&'a DatabaseFact>,
}

/// Joins what the host, Mutagen and the filesystem report into one row per
/// lease. `exists` answers whether a worktree path is still on this machine.
pub fn join(
    config: &Config,
    host: &HostFacts,
    syncs: &[SyncSession],
    exists: impl Fn(&str) -> bool,
) -> Vec<Lease> {
    let mut parts: BTreeMap<String, Parts> = BTreeMap::new();
    for container in &host.containers {
        let lease = container
            .label("com.docker.compose.project")
            .and_then(|project| config.project.lease_of(project));
        if let Some(lease) = lease {
            parts
                .entry(lease.to_string())
                .or_default()
                .containers
                .push(container);
        }
    }
    for session in syncs {
        if let Some(lease) = session.lease(&config.label_keys) {
            parts.entry(lease.to_string()).or_default().sync = Some(session);
        }
    }
    if let Some(db) = &config.database {
        for fact in &host.databases {
            if let Some(lease) = db.name.lease_of(&fact.name) {
                parts.entry(lease.to_string()).or_default().database = Some(fact);
            }
        }
    }

    let mut leases: Vec<Lease> = parts
        .into_iter()
        .map(|(name, parts)| build(config, host, name, parts, &exists))
        .collect();
    // Name order within a group keeps rows still while the list refreshes.
    leases.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
    leases
}

fn build(
    config: &Config,
    host: &HostFacts,
    name: String,
    mut parts: Parts,
    exists: &impl Fn(&str) -> bool,
) -> Lease {
    parts.containers.sort_by(|a, b| a.name.cmp(&b.name));
    let main = main_container(config, &parts.containers);
    let state = state_of(main);

    let path = main
        .and_then(|c| c.label("crumb.worktree"))
        .or_else(|| parts.sync.and_then(SyncSession::local_alpha))
        .or_else(|| match config.host {
            Host::Local => main.and_then(|c| c.label("com.docker.compose.project.working_dir")),
            Host::Ssh(_) => None,
        });
    let worktree = path.map(|path| Worktree {
        path: path.to_string(),
        exists: exists(path),
    });

    let memory: Vec<u64> = parts
        .containers
        .iter()
        .filter_map(|c| host.memory.get(&c.id).copied())
        .collect();

    let sync = parts.sync.map(|s| Sync {
        session: s.name.clone(),
        status: s.status.clone(),
        paused: s.paused,
        connected: s.alpha.connected && s.beta.connected,
        conflicts: s.conflict_count(),
        error: s.last_error.clone().filter(|e| !e.is_empty()),
    });
    let database = parts.database.map(|d| Database {
        name: d.name.clone(),
        comment: d.comment.clone(),
    });

    let gone = worktree.as_ref().is_some_and(|w| !w.exists);
    let reasons = reasons(&state, gone, sync.as_ref());
    let group = if gone {
        Group::Orphaned
    } else if state == State::Absent && sync.is_none() {
        Group::DatabaseOnly
    } else if reasons.is_empty() {
        Group::Running
    } else {
        Group::NeedsYou
    };

    Lease {
        name,
        group,
        container: main.map(|c| c.name.trim_start_matches('/').to_string()),
        port: main.and_then(|c| published_port(config, c)),
        memory_bytes: (!memory.is_empty()).then(|| memory.iter().sum()),
        restarts: main.map_or(0, |c| c.restarts),
        since: main.and_then(since),
        state,
        reasons,
        worktree,
        sync,
        database,
    }
}

fn reasons(state: &State, worktree_gone: bool, sync: Option<&Sync>) -> Vec<Reason> {
    let mut reasons = Vec::new();
    if worktree_gone {
        reasons.push(Reason::WorktreeGone);
    }
    match state {
        State::CrashLoop { .. } => reasons.push(Reason::CrashLoop),
        State::Exited { .. } => reasons.push(Reason::Exited),
        State::Unhealthy => reasons.push(Reason::Unhealthy),
        State::Absent if sync.is_some() => reasons.push(Reason::NoContainers),
        _ => {}
    }
    // An orphan's sync is broken by definition; saying so again is noise.
    if let Some(sync) = sync.filter(|_| !worktree_gone) {
        if sync.paused {
            reasons.push(Reason::SyncPaused);
        } else if !sync.connected {
            reasons.push(Reason::SyncDisconnected);
        }
        if sync.conflicts > 0 {
            reasons.push(Reason::SyncConflicts);
        }
        if sync.error.is_some() {
            reasons.push(Reason::SyncError);
        }
    }
    reasons
}

fn main_container<'a>(config: &Config, containers: &[&'a Container]) -> Option<&'a Container> {
    let by_service = config.service.as_deref().and_then(|service| {
        containers
            .iter()
            .find(|c| c.label("com.docker.compose.service") == Some(service))
    });
    by_service
        .or_else(|| {
            containers
                .iter()
                .find(|c| published_port(config, c).is_some())
        })
        .or_else(|| containers.first())
        .copied()
}

fn state_of(container: Option<&Container>) -> State {
    let Some(c) = container else {
        return State::Absent;
    };
    let health = c.health.as_deref();
    let looping = c.restarts >= CRASH_LOOP_RESTARTS && health != Some("healthy");
    match c.status.as_str() {
        "restarting" => State::CrashLoop {
            restarts: c.restarts,
        },
        "running" if looping => State::CrashLoop {
            restarts: c.restarts,
        },
        "running" => match health {
            Some("healthy") => State::Healthy,
            Some("starting") => State::Starting,
            Some("unhealthy") => State::Unhealthy,
            _ => State::Running,
        },
        "exited" | "dead" if c.exit_code != 0 => State::Exited { code: c.exit_code },
        _ => State::Stopped,
    }
}

fn since(c: &Container) -> Option<Timestamp> {
    let raw = match c.status.as_str() {
        "running" | "restarting" => &c.started_at,
        _ => &c.finished_at,
    };
    // Docker reports "0001-01-01T00:00:00Z" for events that never happened.
    raw.parse::<Timestamp>()
        .ok()
        .filter(|t| *t > Timestamp::UNIX_EPOCH)
}

/// The host port of the main container: its `crumb.port` label, else the
/// binding of the configured container port, preferring loopback.
fn published_port(config: &Config, c: &Container) -> Option<u16> {
    if let Some(port) = c.label("crumb.port").and_then(|p| p.parse().ok()) {
        return Some(port);
    }
    let key = config.port.map(|p| format!("{p}/tcp"));
    [&c.ports, &c.bindings]
        .into_iter()
        .flatten()
        .find_map(|map| {
            let bindings: Vec<&Binding> = match &key {
                Some(key) => map
                    .get(key)
                    .and_then(Option::as_ref)
                    .into_iter()
                    .flatten()
                    .collect(),
                None => {
                    let mut keys: Vec<&String> = map.keys().collect();
                    keys.sort();
                    keys.into_iter()
                        .filter_map(|k| map.get(k).and_then(Option::as_ref))
                        .flatten()
                        .collect()
                }
            };
            bindings
                .iter()
                .find(|b| b.host_ip == "127.0.0.1")
                .or(bindings.first())
                .and_then(|b| b.host_port.parse().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Database as DbConfig, DbServer, Pattern};
    use crate::{mutagen, probe};

    fn config() -> Config {
        Config {
            host: Host::Ssh("devbox".into()),
            mutagen: true,
            label_keys: vec!["crumb.lease".into(), "lymo-worktree".into()],
            project: Pattern::parse("wt_{lease}").unwrap(),
            service: Some("backend".into()),
            port: Some(8080),
            database: Some(DbConfig {
                name: Pattern::parse("wt_{lease}").unwrap(),
                server: DbServer::Docker("db".into()),
                user: "postgres".into(),
            }),
        }
    }

    fn facts() -> (HostFacts, Vec<SyncSession>) {
        (
            probe::parse(include_str!("../tests/fixtures/probe.txt")).unwrap(),
            mutagen::parse(include_str!("../tests/fixtures/mutagen-sync.json")).unwrap(),
        )
    }

    fn container<'a>(host: &'a mut HostFacts, name: &str) -> &'a mut Container {
        host.containers
            .iter_mut()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no container {name}"))
    }

    fn find<'a>(leases: &'a [Lease], name: &str) -> &'a Lease {
        leases.iter().find(|l| l.name == name).unwrap()
    }

    /// Worktrees that still exist in the fixture world.
    fn present(path: &str) -> bool {
        path.ends_with("lym-1119-2")
    }

    #[test]
    fn one_row_per_lease_across_containers_syncs_and_databases() {
        let (host, syncs) = facts();
        let leases = join(&config(), &host, &syncs, present);
        let mut names: Vec<_> = leases.iter().map(|l| l.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "lym_1119",
                "lym_1126",
                "lym_1127",
                "lym_1128",
                "lym_1131",
                "lym_1136",
                "ontology_onboarding",
                "pr919_e2e",
                "sales_ontology_t01",
                "saml_sso"
            ]
        );
    }

    #[test]
    fn a_lease_whose_worktree_is_gone_is_orphaned() {
        let (host, syncs) = facts();
        let leases = join(&config(), &host, &syncs, present);
        let orphan = find(&leases, "lym_1136");
        assert_eq!(orphan.group, Group::Orphaned);
        assert_eq!(orphan.state, State::Healthy);
        assert_eq!(orphan.reasons, [Reason::WorktreeGone]);
        assert_eq!(
            orphan.worktree.as_ref().map(|w| w.path.as_str()),
            Some("/Users/dev/work/lym-1136")
        );
    }

    #[test]
    fn a_database_alone_is_database_only() {
        let (host, syncs) = facts();
        let leases = join(&config(), &host, &syncs, present);
        let db_only = find(&leases, "saml_sso");
        assert_eq!(db_only.group, Group::DatabaseOnly);
        assert_eq!(db_only.state, State::Absent);
        assert!(db_only.reasons.is_empty());
    }

    #[test]
    fn a_healthy_lease_with_its_worktree_is_running() {
        let (mut host, syncs) = facts();
        container(&mut host, "/wt_lym_1119-backend-1").health = Some("healthy".into());
        let leases = join(&config(), &host, &syncs, present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.group, Group::Running);
        assert_eq!(lease.state, State::Healthy);
        assert_eq!(lease.port, Some(8107));
        assert!(lease.memory_bytes.is_some());
    }

    #[test]
    fn an_exited_backend_needs_you() {
        let (mut host, syncs) = facts();
        let backend = container(&mut host, "/wt_lym_1119-backend-1");
        backend.status = "exited".into();
        backend.exit_code = 1;
        backend.finished_at = "2026-09-28T11:39:12Z".into();
        let leases = join(&config(), &host, &syncs, present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.group, Group::NeedsYou);
        assert_eq!(lease.state, State::Exited { code: 1 });
        assert_eq!(lease.reasons, [Reason::Exited]);
        assert_eq!(lease.since, Some("2026-09-28T11:39:12Z".parse().unwrap()));
        // A stopped container keeps its port through the configured binding.
        assert_eq!(lease.port, Some(8107));
    }

    #[test]
    fn many_restarts_without_health_is_a_crash_loop() {
        // Captured as it was: running, health "starting", restarted all week.
        let (host, syncs) = facts();
        let leases = join(&config(), &host, &syncs, present);
        let lease = find(&leases, "lym_1127");
        assert_eq!(lease.state, State::CrashLoop { restarts: 26_279 });
        assert_eq!(lease.group, Group::Orphaned);
        assert_eq!(lease.reasons, [Reason::WorktreeGone, Reason::CrashLoop]);
    }

    #[test]
    fn groups_sort_by_urgency_then_name() {
        let (mut host, syncs) = facts();
        container(&mut host, "/wt_lym_1119-backend-1").status = "restarting".into();
        let leases = join(&config(), &host, &syncs, present);
        assert_eq!(leases[0].group, Group::NeedsYou);
        assert!(
            leases
                .windows(2)
                .all(|pair| { (pair[0].group, &pair[0].name) <= (pair[1].group, &pair[1].name) })
        );
        assert_eq!(leases.last().unwrap().group, Group::DatabaseOnly);
    }

    #[test]
    fn crumb_labels_win_over_derived_values() {
        let (mut host, syncs) = facts();
        let backend = container(&mut host, "/wt_lym_1136-backend-1");
        let labels = backend.labels.get_or_insert_default();
        labels.insert("crumb.port".into(), "8199".into());
        labels.insert("crumb.worktree".into(), "/Users/dev/work/lym-1119-2".into());
        let leases = join(&config(), &host, &syncs, present);
        let lease = find(&leases, "lym_1136");
        assert_eq!(lease.port, Some(8199));
        assert!(lease.worktree.as_ref().is_some_and(|w| w.exists));
        assert!(!lease.reasons.contains(&Reason::WorktreeGone));
    }

    #[test]
    fn sync_problems_on_a_live_worktree_need_you() {
        let (host, mut syncs) = facts();
        let session = syncs.iter_mut().find(|s| s.name == "wt-lym-1119").unwrap();
        session.paused = true;
        let leases = join(&config(), &host, &syncs, present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.group, Group::NeedsYou);
        assert!(lease.reasons.contains(&Reason::SyncPaused));
    }
}
