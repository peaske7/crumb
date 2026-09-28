use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, DbServer};
use crate::run::{Runner, quote};

const SCRIPT: &str = include_str!("probe.sh");

/// Everything the probe reports about the host.
#[derive(Debug, Default, Clone)]
pub struct HostFacts {
    pub mem: Option<Mem>,
    pub containers: Vec<Container>,
    /// Container id to bytes in use, from the cgroup.
    pub memory: HashMap<String, u64>,
    pub listeners: Vec<u16>,
    pub databases: Vec<DatabaseFact>,
    /// Database name to the newest applied migration version.
    pub schema: HashMap<String, String>,
    /// stderr from the host's docker and psql calls, one line each.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mem {
    pub total_mb: u64,
    pub available_mb: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub oom_killed: bool,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    #[serde(default)]
    pub health: Option<String>,
    pub restarts: u64,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub mounts: Option<Vec<Mount>>,
    #[serde(default)]
    pub ports: Option<HashMap<String, Option<Vec<Binding>>>>,
    #[serde(default)]
    pub bindings: Option<HashMap<String, Option<Vec<Binding>>>>,
}

impl Container {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.as_ref()?.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Mount {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Binding {
    #[serde(default)]
    pub host_ip: String,
    #[serde(default)]
    pub host_port: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DatabaseFact {
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
}

pub fn run(config: &Config, runner: &Runner) -> Result<HostFacts> {
    let output = runner.script(&config.host, "probe.sh", &script(config))?;
    let host = config.host.label();
    parse(&String::from_utf8_lossy(&output.stdout)).map_err(|err| {
        // When ssh or bash failed, their stderr says why better than the parser.
        let stderr = String::from_utf8_lossy(&output.stderr);
        match stderr.lines().find(|line| !line.trim().is_empty()) {
            Some(line) => anyhow::anyhow!("{host}: {}", line.trim()),
            None => err.context(format!("probe on {host} failed")),
        }
    })
}

fn script(config: &Config) -> String {
    let (docker, url, user, like) = match &config.database {
        Some(db) => {
            let (docker, url) = match &db.server {
                Some(DbServer::Docker(container)) => (container.as_str(), ""),
                Some(DbServer::Url(url)) => ("", url.as_str()),
                None => ("", ""),
            };
            (docker, url, db.user.as_str(), db.name.sql_like())
        }
        None => ("", "", "postgres", String::new()),
    };
    let schema = config.schema.as_ref().map_or("", |s| s.query.as_str());
    format!(
        "PROJECT_PREFIX={}\nDB_DOCKER={}\nDB_URL={}\nDB_USER={}\nDB_LIKE={}\nSCHEMA_QUERY={}\n{SCRIPT}",
        quote(config.project.prefix()),
        quote(docker),
        quote(url),
        quote(user),
        quote(&like),
        quote(schema),
    )
}

pub fn parse(output: &str) -> Result<HostFacts> {
    let mut facts = HostFacts::default();
    let mut section = "";
    let mut started = false;
    let mut finished = false;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix("@@") {
            section = name;
            started |= name.starts_with("crumb-probe");
            finished |= name == "end";
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match section {
            "mem" => {
                let mut parts = line.split_whitespace().map(str::parse::<u64>);
                if let (Some(Ok(total_mb)), Some(Ok(available_mb))) = (parts.next(), parts.next()) {
                    facts.mem = Some(Mem {
                        total_mb,
                        available_mb,
                    });
                }
            }
            "containers" => facts.containers.push(
                serde_json::from_str(line)
                    .with_context(|| format!("unreadable container line: {line}"))?,
            ),
            "memory" => {
                if let Some((id, bytes)) = line.split_once(' ')
                    && let Ok(bytes) = bytes.trim().parse()
                {
                    facts.memory.insert(id.to_string(), bytes);
                }
            }
            "listeners" => {
                if let Some(port) = line.rsplit(':').next().and_then(|p| p.parse().ok()) {
                    facts.listeners.push(port);
                }
            }
            "databases" => {
                facts.databases = serde_json::from_str(line)
                    .with_context(|| format!("unreadable database list: {line}"))?;
            }
            "schema" => {
                if let Some((db, version)) = line.split_once(' ')
                    && !version.trim().is_empty()
                {
                    facts
                        .schema
                        .insert(db.to_string(), version.trim().to_string());
                }
            }
            "warnings" => facts.warnings.push(line.to_string()),
            _ => {}
        }
    }
    if !started {
        bail!("no probe output");
    }
    if !finished {
        bail!("probe stopped before the end");
    }
    facts.listeners.sort_unstable();
    facts.listeners.dedup();
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/probe.txt");

    #[test]
    fn parses_every_section() {
        let facts = parse(SAMPLE).unwrap();
        assert_eq!(
            facts.mem,
            Some(Mem {
                total_mb: 15992,
                available_mb: 9500
            })
        );
        assert_eq!(facts.containers.len(), 14);
        assert!(facts.memory.len() >= 5);
        assert!(facts.listeners.contains(&8101));
        assert_eq!(facts.databases.len(), 10);
    }

    #[test]
    fn reads_container_state_and_labels() {
        let facts = parse(SAMPLE).unwrap();
        let looping = facts
            .containers
            .iter()
            .find(|c| c.name == "/wt_lym_1127-backend-1")
            .unwrap();
        assert_eq!(looping.restarts, 26279);
        assert_eq!(
            looping.label("com.docker.compose.project"),
            Some("wt_lym_1127")
        );
        assert_eq!(looping.health.as_deref(), Some("starting"));
    }

    #[test]
    fn reads_schema_versions() {
        let facts = parse(
            "@@crumb-probe 1\n@@databases\n[]\n@@schema\nwt_a 20260928220000\nwt_b \n@@end\n",
        )
        .unwrap();
        assert_eq!(
            facts.schema.get("wt_a").map(String::as_str),
            Some("20260928220000")
        );
        assert!(!facts.schema.contains_key("wt_b"));
    }

    #[test]
    fn rejects_truncated_output() {
        let truncated = SAMPLE.split("@@databases").next().unwrap();
        assert!(parse(truncated).is_err());
        assert!(parse("").is_err());
    }
}
