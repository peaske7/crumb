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

/// The settings the read path needs, resolved and validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: Host,
    pub mutagen: bool,
    pub label_keys: Vec<String>,
    pub project: Pattern,
    pub service: Option<String>,
    pub port: Option<u16>,
    pub database: Option<Database>,
}

#[derive(Debug, Clone)]
pub struct Database {
    pub name: Pattern,
    pub server: DbServer,
    pub user: String,
}

#[derive(Debug, Default, Deserialize)]
struct Raw {
    host: Option<String>,
    #[serde(default)]
    code: RawCode,
    #[serde(default)]
    runtime: RawRuntime,
    database: Option<RawDatabase>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCode {
    sync: Option<String>,
    label_keys: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRuntime {
    project: Option<String>,
    service: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct RawDatabase {
    name: Option<String>,
    server: Option<String>,
    user: Option<String>,
}

pub const PROJECT_FILE: &str = "crumb.toml";

/// Loads `~/.config/crumb/config.toml`, then the project's `crumb.toml` over it,
/// then `CRUMB_HOST` and the `--host` flag over both.
pub fn load(explicit: Option<&Path>, host_flag: Option<&str>) -> Result<Config> {
    let mut table = toml::Table::new();
    if let Some(user) = user_config_path().filter(|p| p.is_file()) {
        merge(&mut table, read_table(&user)?);
    }
    let source = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => find_project_config(&std::env::current_dir()?),
    };
    if let Some(path) = &source {
        merge(&mut table, read_table(path)?);
    }
    let mut raw: Raw = table
        .try_into()
        .context("crumb.toml does not match the expected shape")?;
    if let Some(host) = host_flag
        .map(str::to_string)
        .or_else(|| std::env::var("CRUMB_HOST").ok())
    {
        raw.host = Some(host);
    }
    resolve(raw)
}

fn resolve(raw: Raw) -> Result<Config> {
    let mutagen = match raw.code.sync.as_deref() {
        None | Some("none") => false,
        Some("mutagen") => true,
        Some(other) => bail!("code.sync must be \"mutagen\" or \"none\", got {other:?}"),
    };
    let database = raw
        .database
        .map(|db| -> Result<Database> {
            Ok(Database {
                name: Pattern::parse(db.name.as_deref().unwrap_or("crumb_{lease}"))?,
                server: DbServer::parse(
                    db.server
                        .as_deref()
                        .context("database.server is required to list lease databases")?,
                )?,
                user: db.user.unwrap_or_else(|| "postgres".to_string()),
            })
        })
        .transpose()?;
    Ok(Config {
        host: Host::parse(raw.host.as_deref().unwrap_or("local"))?,
        mutagen,
        label_keys: raw
            .code
            .label_keys
            .unwrap_or_else(|| vec!["crumb.lease".to_string()]),
        project: Pattern::parse(raw.runtime.project.as_deref().unwrap_or("crumb_{lease}"))?,
        service: raw.runtime.service,
        port: raw.runtime.port,
        database,
    })
}

fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("crumb").join("config.toml"))
}

fn find_project_config(start: &Path) -> Option<PathBuf> {
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
    fn project_config_overrides_user_config() {
        let mut base: toml::Table = "host = \"ssh://a\"\n[runtime]\nservice = \"api\"\nport = 80"
            .parse()
            .unwrap();
        let over: toml::Table = "[runtime]\nport = 8080".parse().unwrap();
        merge(&mut base, over);
        let raw: Raw = base.try_into().unwrap();
        assert_eq!(raw.host.as_deref(), Some("ssh://a"));
        assert_eq!(raw.runtime.service.as_deref(), Some("api"));
        assert_eq!(raw.runtime.port, Some(8080));
    }
}
