use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::Serialize;

use crate::config::{Config, Forward, Host};
use crate::mutagen::{ForwardSession, SyncSession};
use crate::probe::{Binding, Container, DatabaseFact, HostFacts};
use crate::template::Vars;
use crate::wire;

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
    /// Files in the worktree changed after the backend started.
    RestartPending,
    /// The worktree's lockfile differs from the one in the image.
    DepsBehind,
    /// The worktree has migrations the lease database has not applied.
    SchemaBehind,
    /// Nothing forwards the lease's port to this machine.
    NoForward,
    /// A wired env file is missing or points somewhere else.
    WiringStale,
    /// A wired env file points at a non-loopback host.
    WiringRemote,
}

impl Reason {
    /// Soft reasons mean "behind", not "broken": the lease stays in its group
    /// and says so in its status.
    pub fn is_soft(&self) -> bool {
        matches!(
            self,
            Reason::RestartPending
                | Reason::DepsBehind
                | Reason::SchemaBehind
                | Reason::NoForward
                | Reason::WiringStale
                | Reason::WiringRemote
        )
    }
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
    /// The main container's image id.
    pub image: Option<String>,
    /// Worktree paths the backend bind-mounts, so edits there reach it.
    pub watch: Vec<String>,
    /// The first file that changed after the backend started, relative to the worktree.
    pub changed_file: Option<String>,
    /// When the image the backend runs was built, once it is known.
    pub image_built: Option<Timestamp>,
    /// The port on the host.
    pub port: Option<u16>,
    /// The port on this machine: the forward's, or the host's when direct.
    pub local_port: Option<u16>,
    pub memory_bytes: Option<u64>,
    pub restarts: u64,
    /// When the main container started (running) or stopped (otherwise).
    pub since: Option<Timestamp>,
    pub sync: Option<Sync>,
    pub forward: Option<PortForward>,
    pub database: Option<Database>,
    /// The newest migration in the worktree, when the schema check runs.
    pub newest_migration: Option<String>,
    pub wiring: Vec<Wiring>,
    /// The worktree crumb was started in, listed even before it has a lease.
    pub here: bool,
}

impl Lease {
    pub fn new(name: &str) -> Self {
        Lease {
            name: name.to_string(),
            group: Group::Running,
            state: State::Absent,
            reasons: Vec::new(),
            worktree: None,
            container: None,
            image: None,
            watch: Vec::new(),
            changed_file: None,
            image_built: None,
            port: None,
            local_port: None,
            memory_bytes: None,
            restarts: 0,
            since: None,
            sync: None,
            forward: None,
            database: None,
            newest_migration: None,
            wiring: Vec::new(),
            here: false,
        }
    }

    pub fn worktree_gone(&self) -> bool {
        self.worktree.as_ref().is_some_and(|w| !w.exists)
    }

    /// The worktree path when it still exists on this machine.
    pub fn live_worktree(&self) -> Option<&str> {
        self.worktree
            .as_ref()
            .filter(|w| w.exists)
            .map(|w| w.path.as_str())
    }

    /// Whether the backend is up in some form, so its checks mean something.
    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            State::Healthy | State::Running | State::Starting | State::Unhealthy
        )
    }

    /// Template values for this lease's commands and wired files.
    pub fn vars(&self, config: &Config) -> Vars {
        let mut vars = Vars::default();
        vars.set("lease", self.name.as_str())
            .set("host", config.host.label());
        if let Some(worktree) = &self.worktree {
            vars.set("worktree", worktree.path.as_str());
        }
        if let Some(port) = self.port {
            vars.set("port", port.to_string());
            if let Some(nn) = port.checked_sub(config.host_base).filter(|nn| *nn < 100) {
                vars.set("n", format!("{nn:02}"));
            }
        }
        if let Some(port) = self.local_port {
            vars.set("local_port", port.to_string());
        }
        let database = self.database.as_ref().map(|d| d.name.clone()).or_else(|| {
            config
                .database
                .as_ref()
                .map(|db| db.name.render(&self.name))
        });
        if let Some(database) = database {
            vars.set("database", database);
        }
        vars
    }

    /// Places the lease by what to do about it. Call again after adding reasons.
    pub fn regroup(&mut self) {
        self.group = if self.worktree_gone() {
            Group::Orphaned
        } else if self.state == State::Absent && self.sync.is_none() && !self.here {
            Group::DatabaseOnly
        } else if self.reasons.iter().any(|r| !r.is_soft()) {
            Group::NeedsYou
        } else {
            Group::Running
        };
    }
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
pub struct PortForward {
    pub session: String,
    pub local_port: Option<u16>,
    pub remote_port: Option<u16>,
    pub up: bool,
    pub paused: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Database {
    pub name: String,
    pub comment: Option<String>,
    /// The newest migration version applied, when the schema check runs.
    pub applied: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Wiring {
    pub file: String,
    pub status: wire::Status,
}

#[derive(Default)]
struct Parts<'a> {
    containers: Vec<&'a Container>,
    sync: Option<&'a SyncSession>,
    forward: Option<&'a ForwardSession>,
    database: Option<&'a DatabaseFact>,
}

/// Joins what the host, Mutagen and the filesystem report into one row per
/// lease. `exists` answers whether a worktree path is still on this machine.
pub fn join(
    config: &Config,
    host: &HostFacts,
    syncs: &[SyncSession],
    forwards: &[ForwardSession],
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
    for session in forwards {
        if let Some(lease) = session.lease(&config.label_keys) {
            parts.entry(lease.to_string()).or_default().forward = Some(session);
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
        applied: host.schema.get(&d.name).cloned(),
    });
    let forward = parts.forward.map(|f| PortForward {
        session: f.name.clone(),
        local_port: f.source.port(),
        remote_port: f.destination.port(),
        up: f.up(),
        paused: f.paused,
        error: f.last_error.clone().filter(|e| !e.is_empty()),
    });
    let port = main.and_then(|c| published_port(config, c));
    let local_port = match config.forward {
        Forward::Direct => port,
        Forward::Mutagen => forward.as_ref().and_then(|f| f.local_port),
    };

    let gone = worktree.as_ref().is_some_and(|w| !w.exists);
    let mut reasons = reasons(&state, gone, sync.as_ref());
    let forward_down = forward
        .as_ref()
        .is_none_or(|f| !f.up || f.remote_port != port);
    if config.forward == Forward::Mutagen
        && !gone
        && port.is_some()
        && running(&state)
        && forward_down
    {
        reasons.push(Reason::NoForward);
    }

    let mut lease = Lease {
        name,
        group: Group::Running,
        container: main.map(|c| c.name.trim_start_matches('/').to_string()),
        image: main.and_then(|c| c.image.clone()),
        watch: main.map(watched_paths).unwrap_or_default(),
        changed_file: None,
        image_built: None,
        port,
        local_port,
        memory_bytes: (!memory.is_empty()).then(|| memory.iter().sum()),
        restarts: main.map_or(0, |c| c.restarts),
        since: main.and_then(since),
        state,
        reasons,
        worktree,
        sync,
        forward,
        database,
        newest_migration: None,
        wiring: Vec::new(),
        here: false,
    };
    lease.regroup();
    lease
}

fn running(state: &State) -> bool {
    matches!(
        state,
        State::Healthy | State::Running | State::Starting | State::Unhealthy
    )
}

/// The main container's bind mounts that come from its project directory,
/// relative to it: `apps/backend/src`, `packages`. These are the worktree
/// paths whose edits the backend sees.
fn watched_paths(c: &Container) -> Vec<String> {
    let Some(root) = c.label("com.docker.compose.project.working_dir") else {
        return Vec::new();
    };
    let root = format!("{}/", root.trim_end_matches('/'));
    let mut paths: Vec<String> = c
        .mounts
        .iter()
        .flatten()
        .filter(|m| m.kind == "bind")
        .filter_map(|m| m.source.strip_prefix(&root))
        .filter(|relative| !relative.is_empty())
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths
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
        // 137 and 143 are SIGKILL and SIGTERM: `docker stop`, unless the
        // kernel killed it for memory.
        "exited" if matches!(c.exit_code, 137 | 143) && !c.oom_killed => State::Stopped,
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
    use crate::{mutagen, probe};

    fn config() -> Config {
        Config::parse(
            r#"
            host = "ssh://devbox"
            [code]
            sync = "mutagen"
            label_keys = ["crumb.lease", "lymo-worktree"]
            [runtime]
            project = "wt_{lease}"
            service = "backend"
            port = 8080
            [ports]
            forward = "direct"
            [database]
            name = "wt_{lease}"
            server = "docker://db"
            "#,
        )
        .unwrap()
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
        let leases = join(&config(), &host, &syncs, &[], present);
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
        let leases = join(&config(), &host, &syncs, &[], present);
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
        let leases = join(&config(), &host, &syncs, &[], present);
        let db_only = find(&leases, "saml_sso");
        assert_eq!(db_only.group, Group::DatabaseOnly);
        assert_eq!(db_only.state, State::Absent);
        assert!(db_only.reasons.is_empty());
    }

    #[test]
    fn a_healthy_lease_with_its_worktree_is_running() {
        let (mut host, syncs) = facts();
        container(&mut host, "/wt_lym_1119-backend-1").health = Some("healthy".into());
        let leases = join(&config(), &host, &syncs, &[], present);
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
        let leases = join(&config(), &host, &syncs, &[], present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.group, Group::NeedsYou);
        assert_eq!(lease.state, State::Exited { code: 1 });
        assert_eq!(lease.reasons, [Reason::Exited]);
        assert_eq!(lease.since, Some("2026-09-28T11:39:12Z".parse().unwrap()));
        // A stopped container keeps its port through the configured binding.
        assert_eq!(lease.port, Some(8107));
    }

    #[test]
    fn a_backend_killed_by_docker_stop_is_stopped_not_exited() {
        let (mut host, syncs) = facts();
        let backend = container(&mut host, "/wt_lym_1119-backend-1");
        backend.status = "exited".into();
        backend.exit_code = 137;
        let leases = join(&config(), &host, &syncs, &[], present);
        assert_eq!(find(&leases, "lym_1119").state, State::Stopped);
        container(&mut host, "/wt_lym_1119-backend-1").oom_killed = true;
        let leases = join(&config(), &host, &syncs, &[], present);
        assert_eq!(find(&leases, "lym_1119").state, State::Exited { code: 137 });
    }

    #[test]
    fn many_restarts_without_health_is_a_crash_loop() {
        // Captured as it was: running, health "starting", restarted all week.
        let (host, syncs) = facts();
        let leases = join(&config(), &host, &syncs, &[], present);
        let lease = find(&leases, "lym_1127");
        assert_eq!(lease.state, State::CrashLoop { restarts: 26_279 });
        assert_eq!(lease.group, Group::Orphaned);
        assert_eq!(lease.reasons, [Reason::WorktreeGone, Reason::CrashLoop]);
    }

    #[test]
    fn groups_sort_by_urgency_then_name() {
        let (mut host, syncs) = facts();
        container(&mut host, "/wt_lym_1119-backend-1").status = "restarting".into();
        let leases = join(&config(), &host, &syncs, &[], present);
        assert_eq!(leases[0].group, Group::NeedsYou);
        assert!(
            leases
                .windows(2)
                .all(|pair| { (pair[0].group, &pair[0].name) <= (pair[1].group, &pair[1].name) })
        );
        assert_eq!(leases.last().unwrap().group, Group::DatabaseOnly);
    }

    #[test]
    fn watches_only_bind_mounts_from_the_project_directory() {
        let (mut host, syncs) = facts();
        let mount = |kind: &str, source: &str| crate::probe::Mount {
            kind: kind.into(),
            source: source.into(),
        };
        container(&mut host, "/wt_lym_1119-backend-1").mounts = Some(vec![
            mount("bind", "/home/dev/app/certs"),
            mount("bind", "/home/dev/wt/lym_1119/packages"),
            mount("bind", "/home/dev/wt/lym_1119/apps/backend/src"),
            mount("volume", "/var/lib/docker/volumes/x/_data"),
        ]);
        let leases = join(&config(), &host, &syncs, &[], present);
        assert_eq!(
            find(&leases, "lym_1119").watch,
            ["apps/backend/src", "packages"]
        );
    }

    #[test]
    fn crumb_labels_win_over_derived_values() {
        let (mut host, syncs) = facts();
        let backend = container(&mut host, "/wt_lym_1136-backend-1");
        let labels = backend.labels.get_or_insert_default();
        labels.insert("crumb.port".into(), "8199".into());
        labels.insert("crumb.worktree".into(), "/Users/dev/work/lym-1119-2".into());
        let leases = join(&config(), &host, &syncs, &[], present);
        let lease = find(&leases, "lym_1136");
        assert_eq!(lease.port, Some(8199));
        assert!(lease.worktree.as_ref().is_some_and(|w| w.exists));
        assert!(!lease.reasons.contains(&Reason::WorktreeGone));
    }

    #[test]
    fn a_running_lease_without_its_forward_is_behind() {
        let (mut host, syncs) = facts();
        container(&mut host, "/wt_lym_1119-backend-1").health = Some("healthy".into());
        let mut config = config();
        config.forward = Forward::Mutagen;
        let leases = join(&config, &host, &syncs, &[], present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.reasons, [Reason::NoForward]);
        assert_eq!(lease.group, Group::Running);
        assert_eq!(lease.local_port, None);

        let forwards =
            mutagen::parse(include_str!("../tests/fixtures/mutagen-forward.json")).unwrap();
        let leases = join(&config, &host, &syncs, &forwards, present);
        let lease = find(&leases, "lym_1119");
        assert!(lease.reasons.is_empty());
        assert_eq!(lease.local_port, Some(18107));
    }

    #[test]
    fn sync_problems_on_a_live_worktree_need_you() {
        let (host, mut syncs) = facts();
        let session = syncs.iter_mut().find(|s| s.name == "wt-lym-1119").unwrap();
        session.paused = true;
        let leases = join(&config(), &host, &syncs, &[], present);
        let lease = find(&leases, "lym_1119");
        assert_eq!(lease.group, Group::NeedsYou);
        assert!(lease.reasons.contains(&Reason::SyncPaused));
    }
}
