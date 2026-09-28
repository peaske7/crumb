use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Where lease backends run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Local,
    Ssh(String),
}

impl Host {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "" | "local" => Ok(Host::Local),
            _ => match value.strip_prefix("ssh://") {
                Some(alias) if !alias.is_empty() => Ok(Host::Ssh(alias.to_string())),
                _ => bail!("host must be \"local\" or \"ssh://<host>\", got {value:?}"),
            },
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Host::Local => "local",
            Host::Ssh(alias) => alias,
        }
    }

    pub fn is_local(&self) -> bool {
        *self == Host::Local
    }
}

/// A name template with exactly one `{lease}` placeholder, such as `wt_{lease}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    prefix: String,
    suffix: String,
}

impl Pattern {
    pub fn parse(template: &str) -> Result<Self> {
        let Some((prefix, suffix)) = template.split_once("{lease}") else {
            bail!("template {template:?} needs a {{lease}} placeholder");
        };
        if suffix.contains("{lease}") {
            bail!("template {template:?} has more than one {{lease}} placeholder");
        }
        Ok(Self {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn lease_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(&self.prefix)?
            .strip_suffix(&self.suffix)
            .filter(|lease| !lease.is_empty())
    }

    pub fn render(&self, lease: &str) -> String {
        format!("{}{lease}{}", self.prefix, self.suffix)
    }

    /// A SQL `LIKE` pattern matching every rendering of the template.
    pub fn sql_like(&self) -> String {
        let escape = |part: &str| {
            part.replace('\\', "\\\\")
                .replace('_', "\\_")
                .replace('%', "\\%")
                .replace('\'', "''")
        };
        format!("{}%{}", escape(&self.prefix), escape(&self.suffix))
    }
}

/// Where the Postgres server that holds lease databases can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbServer {
    Docker(String),
    Url(String),
}

impl DbServer {
    fn parse(value: &str) -> Result<Self> {
        if let Some(container) = value.strip_prefix("docker://") {
            return Ok(DbServer::Docker(container.to_string()));
        }
        if value.starts_with("postgres://") || value.starts_with("postgresql://") {
            return Ok(DbServer::Url(value.to_string()));
        }
        bail!(
            "database.server must be \"docker://<container>\" or a postgres:// URL, got {value:?}"
        )
    }
}

/// How the lease's backend runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    /// Docker Compose on the host, one project per lease.
    Compose {
        /// The compose file, relative to the repository.
        file: Option<String>,
        /// Passed to compose as `--env-file`, a path on the host.
        env_file: Option<String>,
        /// Extra arguments for `docker compose up -d`.
        up_args: Vec<String>,
    },
    /// A command in a tmux session named after the lease.
    Process {
        start: String,
        /// A URL that answers once the process is ready.
        ready: Option<String>,
    },
}

/// How this machine reaches a lease's port on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forward {
    /// A Mutagen forward session from a loopback port here to loopback there.
    Mutagen,
    /// The host port as is: the host is this machine, or reachable directly.
    Direct,
}

/// Which paths the code sync leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignore {
    /// The `sync.defaults.ignore.paths` of a mutagen.yml, relative to the
    /// repository (looked up in the worktree, then the main checkout).
    File(String),
    Paths(Vec<String>),
}

/// The settings crumb needs, resolved and validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: Host,
    pub mutagen: bool,
    /// Where replicas live on the host: `<root>/<lease>`.
    pub code_root: String,
    pub ignore: Ignore,
    pub label_keys: Vec<String>,
    pub project: Pattern,
    pub service: Option<String>,
    /// The port the service listens on inside its container.
    pub port: Option<u16>,
    pub runtime: Runtime,
    /// Peak memory of one lease; `up` refuses when it would leave less than
    /// `keep_free_mb` available on the host.
    pub memory_mb: Option<u64>,
    pub keep_free_mb: Option<u64>,
    pub host_base: u16,
    pub local_base: u16,
    pub forward: Forward,
    pub database: Option<Database>,
    pub deps: Option<DepsCheck>,
    pub schema: Option<SchemaCheck>,
    pub wires: Vec<Wire>,
    /// The repository whose crumb.toml was read.
    pub repo: Option<PathBuf>,
}

impl Config {
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self> {
        let table: toml::Table = text.parse().context("parsing config")?;
        let raw: Raw = table
            .try_into()
            .context("crumb.toml does not match the expected shape")?;
        resolve(raw, None)
    }

    /// The Mutagen session for a lease: `wt_{lease}` becomes `wt-lym-1119`.
    /// Mutagen names allow letters, digits and dashes only.
    pub fn sync_name(&self, lease: &str) -> String {
        self.project.render(lease).replace(['_', '.'], "-")
    }

    pub fn forward_name(&self, lease: &str) -> String {
        format!("{}-port", self.sync_name(lease))
    }

    /// The local port that goes with a host port: same `nn`, other base.
    pub fn local_port(&self, host_port: u16) -> u16 {
        match self.forward {
            Forward::Direct => host_port,
            Forward::Mutagen => host_port
                .checked_sub(self.host_base)
                .and_then(|nn| self.local_base.checked_add(nn))
                .unwrap_or(host_port),
        }
    }
}

/// Compare the worktree's lockfile with the one baked into the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepsCheck {
    /// Relative to the worktree, such as `pnpm-lock.yaml`.
    pub lockfile: String,
    /// Inside the image, such as `/app/pnpm-lock.yaml`.
    pub image_path: String,
}

/// Compare the newest migration in the worktree with the newest one applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCheck {
    /// A directory of migration files whose names start with their version.
    pub migrations: String,
    /// SQL that returns the newest applied version, run in the lease database.
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct Database {
    pub name: Pattern,
    /// Where lease databases are listed; without it crumb cannot see them.
    pub server: Option<DbServer>,
    pub user: String,
    /// Commands run on this machine in the worktree (see the command contract).
    pub create: Option<String>,
    pub drop: Option<String>,
    pub migrate: Option<String>,
    /// The built-in driver copies this database when there is no create command.
    pub from: Option<String>,
}

/// An env file crumb writes in the worktree so apps there reach the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wire {
    /// Relative to the worktree. crumb owns the whole file.
    pub file: String,
    /// Copied from the main checkout when missing, such as the base `.env`
    /// the wired file layers over.
    pub seed: Option<String>,
    /// A first line that marks a file an older tool wrote; crumb may replace it.
    pub adopt: Option<String>,
    pub set: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    host: Option<String>,
    #[serde(default)]
    code: RawCode,
    #[serde(default)]
    runtime: RawRuntime,
    #[serde(default)]
    ports: RawPorts,
    database: Option<RawDatabase>,
    #[serde(default)]
    checks: RawChecks,
    #[serde(default)]
    wire: Vec<RawWire>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChecks {
    deps: Option<RawDeps>,
    schema: Option<RawSchema>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeps {
    lockfile: String,
    image_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    migrations: String,
    query: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCode {
    sync: Option<String>,
    root: Option<String>,
    ignore: Option<RawIgnore>,
    label_keys: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawIgnore {
    File(String),
    Paths(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    compose: Option<String>,
    project: Option<String>,
    service: Option<String>,
    port: Option<u16>,
    env_file: Option<String>,
    up_args: Option<Vec<String>>,
    start: Option<String>,
    ready: Option<String>,
    memory_mb: Option<u64>,
    keep_free_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPorts {
    host_base: Option<u16>,
    local_base: Option<u16>,
    forward: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDatabase {
    name: Option<String>,
    server: Option<String>,
    user: Option<String>,
    create: Option<String>,
    drop: Option<String>,
    migrate: Option<String>,
    from: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWire {
    file: String,
    seed: Option<String>,
    adopt: Option<String>,
    #[serde(default)]
    set: BTreeMap<String, String>,
}

pub const PROJECT_FILE: &str = "crumb.toml";

/// Loads the project's `crumb.toml`, then `~/.config/crumb/config.toml` over
/// it, then `CRUMB_HOST` and the `--host` flag over both. The user config
/// holds what differs by machine, such as the host. A worktree whose branch
/// predates crumb.toml uses the main checkout's.
pub fn load(
    explicit: Option<&Path>,
    host_flag: Option<&str>,
    main: Option<&Path>,
) -> Result<Config> {
    let mut table = toml::Table::new();
    let source = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => find_project_config(&std::env::current_dir()?).or_else(|| {
            main.map(|m| m.join(PROJECT_FILE))
                .filter(|path| path.is_file())
        }),
    };
    if let Some(path) = &source {
        merge(&mut table, read_table(path)?);
    }
    if let Some(user) = user_config_path().filter(|p| p.is_file()) {
        merge(&mut table, read_table(&user)?);
    }
    let mut raw: Raw = table.try_into().with_context(|| match &source {
        Some(path) => format!("{} does not match the expected shape", path.display()),
        None => "the config does not match the expected shape".to_string(),
    })?;
    if let Some(host) = host_flag
        .map(str::to_string)
        .or_else(|| std::env::var("CRUMB_HOST").ok())
    {
        raw.host = Some(host);
    }
    let repo = source
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    resolve(raw, repo)
}

fn resolve(raw: Raw, repo: Option<PathBuf>) -> Result<Config> {
    let host = Host::parse(raw.host.as_deref().unwrap_or("local"))?;
    let mutagen = match raw.code.sync.as_deref() {
        None | Some("none") => false,
        Some("mutagen") => true,
        Some(other) => bail!("code.sync must be \"mutagen\" or \"none\", got {other:?}"),
    };
    if mutagen && host.is_local() {
        bail!("code.sync = \"mutagen\" needs a remote host; a local host runs the worktree itself");
    }
    let database = raw
        .database
        .map(|db| -> Result<Database> {
            Ok(Database {
                name: Pattern::parse(db.name.as_deref().unwrap_or("crumb_{lease}"))?,
                server: db.server.as_deref().map(DbServer::parse).transpose()?,
                user: db.user.unwrap_or_else(|| "postgres".to_string()),
                create: db.create,
                drop: db.drop,
                migrate: db.migrate,
                from: db.from,
            })
        })
        .transpose()?;
    if let Some(db) = &database
        && db.create.is_none()
        && db.from.is_some()
        && db.server.is_none()
    {
        bail!("database.from copies a database on database.server; set the server too");
    }
    let runtime = match raw.runtime.start {
        Some(start) => Runtime::Process {
            start,
            ready: raw.runtime.ready,
        },
        None => Runtime::Compose {
            file: raw.runtime.compose,
            env_file: raw.runtime.env_file,
            up_args: raw.runtime.up_args.unwrap_or_default(),
        },
    };
    let forward = match raw.ports.forward.as_deref() {
        None if host.is_local() => Forward::Direct,
        None | Some("mutagen") if !host.is_local() => Forward::Mutagen,
        None | Some("mutagen") => bail!("ports.forward = \"mutagen\" needs a remote host"),
        Some("direct") => Forward::Direct,
        Some(other) => bail!("ports.forward must be \"mutagen\" or \"direct\", got {other:?}"),
    };
    let host_base = raw.ports.host_base.unwrap_or(8100);
    let local_base = match forward {
        Forward::Direct => host_base,
        Forward::Mutagen => raw
            .ports
            .local_base
            .unwrap_or_else(|| host_base.saturating_add(10_000)),
    };
    Ok(Config {
        mutagen,
        code_root: raw.code.root.unwrap_or_else(|| "~/crumb".to_string()),
        ignore: match raw.code.ignore {
            Some(RawIgnore::File(file)) => Ignore::File(file),
            Some(RawIgnore::Paths(paths)) => Ignore::Paths(paths),
            None => Ignore::Paths(Vec::new()),
        },
        label_keys: raw
            .code
            .label_keys
            .unwrap_or_else(|| vec!["crumb.lease".to_string()]),
        project: Pattern::parse(raw.runtime.project.as_deref().unwrap_or("crumb_{lease}"))?,
        service: raw.runtime.service,
        port: raw.runtime.port,
        runtime,
        memory_mb: raw.runtime.memory_mb,
        keep_free_mb: raw.runtime.keep_free_mb,
        host_base,
        local_base,
        forward,
        database,
        deps: raw.checks.deps.map(|deps| DepsCheck {
            lockfile: deps.lockfile,
            image_path: deps.image_path,
        }),
        schema: raw.checks.schema.map(|schema| SchemaCheck {
            migrations: schema.migrations,
            query: schema.query,
        }),
        wires: raw
            .wire
            .into_iter()
            .map(|w| Wire {
                file: w.file,
                seed: w.seed,
                adopt: w.adopt,
                set: w.set,
            })
            .collect(),
        repo,
        host,
    })
}

pub fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("crumb").join("config.toml"))
}

pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(PROJECT_FILE))
        .find(|candidate| candidate.is_file())
}

fn read_table(path: &Path) -> Result<toml::Table> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.parse::<toml::Table>()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Overlays `over` onto `base`, merging nested tables key by key.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_extracts_the_lease_name() {
        let pattern = Pattern::parse("wt_{lease}").unwrap();
        assert_eq!(pattern.lease_of("wt_lym_1119"), Some("lym_1119"));
        assert_eq!(pattern.lease_of("wt_"), None);
        assert_eq!(pattern.lease_of("postgres"), None);
        assert_eq!(pattern.render("lym_1119"), "wt_lym_1119");
    }

    #[test]
    fn pattern_with_suffix() {
        let pattern = Pattern::parse("app_{lease}_dev").unwrap();
        assert_eq!(pattern.lease_of("app_feature_dev"), Some("feature"));
        assert_eq!(pattern.lease_of("app_feature"), None);
    }

    #[test]
    fn pattern_rejects_missing_or_repeated_placeholder() {
        assert!(Pattern::parse("wt_").is_err());
        assert!(Pattern::parse("{lease}_{lease}").is_err());
    }

    #[test]
    fn sql_like_escapes_wildcards() {
        assert_eq!(Pattern::parse("wt_{lease}").unwrap().sql_like(), "wt\\_%");
    }

    #[test]
    fn host_parsing() {
        assert_eq!(Host::parse("local").unwrap(), Host::Local);
        assert_eq!(
            Host::parse("ssh://indigo").unwrap(),
            Host::Ssh("indigo".into())
        );
        assert!(Host::parse("indigo").is_err());
    }

    #[test]
    fn user_config_overrides_the_project() {
        let mut base: toml::Table = "host = \"ssh://a\"\n[runtime]\nservice = \"api\"\nport = 80"
            .parse()
            .unwrap();
        let over: toml::Table = "host = \"ssh://b\"".parse().unwrap();
        merge(&mut base, over);
        let raw: Raw = base.try_into().unwrap();
        assert_eq!(raw.host.as_deref(), Some("ssh://b"));
        assert_eq!(raw.runtime.service.as_deref(), Some("api"));
        assert_eq!(raw.runtime.port, Some(80));
    }

    #[test]
    fn a_remote_host_forwards_through_mutagen_by_default() {
        let config = Config::parse(
            "host = \"ssh://box\"\n[code]\nsync = \"mutagen\"\n[runtime]\nproject = \"wt_{lease}\"",
        )
        .unwrap();
        assert_eq!(config.forward, Forward::Mutagen);
        assert_eq!((config.host_base, config.local_base), (8100, 18100));
        assert_eq!(config.local_port(8107), 18107);
        assert_eq!(config.sync_name("lym_1119"), "wt-lym-1119");
        assert_eq!(config.forward_name("lym_1119"), "wt-lym-1119-port");
    }

    #[test]
    fn a_local_host_uses_its_ports_directly() {
        let config = Config::parse("[ports]\nhost_base = 3000").unwrap();
        assert_eq!(config.forward, Forward::Direct);
        assert_eq!(config.local_port(3004), 3004);
        assert!(Config::parse("[code]\nsync = \"mutagen\"").is_err());
    }

    #[test]
    fn a_start_command_means_the_process_runtime() {
        let config = Config::parse(
            "[runtime]\nstart = \"pnpm dev --port {port}\"\nready = \"http://localhost:{port}/\"",
        )
        .unwrap();
        assert!(matches!(config.runtime, Runtime::Process { .. }));
    }

    #[test]
    fn wires_and_checks() {
        let config = Config::parse(
            r#"
            [checks]
            schema = { migrations = "db/migrations", query = "select 1" }

            [[wire]]
            file = "apps/web/.env.development.local"
            seed = "apps/web/.env.local"
            set.API_URL = "http://127.0.0.1:{local_port}"
            "#,
        )
        .unwrap();
        assert_eq!(config.wires.len(), 1);
        assert_eq!(
            config.wires[0].set.get("API_URL").map(String::as_str),
            Some("http://127.0.0.1:{local_port}")
        );
        assert!(config.schema.is_some());
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(Config::parse("[runtime]\nservce = \"x\"").is_err());
    }
}
