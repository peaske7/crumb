use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::run::{Runner, quote};

/// One Mutagen synchronization session, as `mutagen sync list --template` prints it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSession {
    #[serde(default)]
    pub identifier: String,
    pub name: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub paused: bool,
    pub status: String,
    #[serde(default)]
    pub successful_cycles: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    pub alpha: Endpoint,
    pub beta: Endpoint,
    #[serde(default)]
    pub conflicts: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    #[serde(default)]
    pub protocol: String,
    pub path: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub staging_progress: Option<Staging>,
}

/// How far a first copy has got, while files are staged.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Staging {
    #[serde(default)]
    pub received_files: u64,
    #[serde(default)]
    pub expected_files: u64,
    #[serde(default)]
    pub total_received_size: u64,
}

impl SyncSession {
    /// The lease this session belongs to, from the first matching label key.
    pub fn lease(&self, label_keys: &[String]) -> Option<&str> {
        lease_label(self.labels.as_ref(), label_keys)
    }

    /// The worktree path when the alpha side is on this machine.
    pub fn local_alpha(&self) -> Option<&str> {
        (self.alpha.protocol == "local").then_some(self.alpha.path.as_str())
    }

    pub fn conflict_count(&self) -> usize {
        self.conflicts.as_ref().map_or(0, Vec::len)
    }
}

/// One Mutagen forward session, as `mutagen forward list --template` prints it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardSession {
    pub name: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub last_error: Option<String>,
    pub source: ForwardEndpoint,
    pub destination: ForwardEndpoint,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForwardEndpoint {
    /// `tcp:127.0.0.1:18107`
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub connected: bool,
}

impl ForwardEndpoint {
    pub fn port(&self) -> Option<u16> {
        self.endpoint.rsplit(':').next()?.parse().ok()
    }
}

impl ForwardSession {
    pub fn lease(&self, label_keys: &[String]) -> Option<&str> {
        lease_label(self.labels.as_ref(), label_keys)
    }

    /// Forwarding with both ends connected.
    pub fn up(&self) -> bool {
        !self.paused && self.source.connected && self.destination.connected
    }
}

fn lease_label<'a>(
    labels: Option<&'a HashMap<String, String>>,
    label_keys: &[String],
) -> Option<&'a str> {
    let labels = labels?;
    label_keys
        .iter()
        .find_map(|key| labels.get(key))
        .map(String::as_str)
}

pub fn list(runner: &Runner) -> Result<Vec<SyncSession>> {
    list_json(runner, "sync", None)
}

pub fn list_forwards(runner: &Runner) -> Result<Vec<ForwardSession>> {
    list_json(runner, "forward", None)
}

/// One sync session by identifier or name, freshly read.
pub fn sync_session(runner: &Runner, id: &str) -> Result<Option<SyncSession>> {
    Ok(list_json(runner, "sync", Some(id))?.into_iter().next())
}

fn list_json<T: DeserializeOwned>(
    runner: &Runner,
    kind: &str,
    session: Option<&str>,
) -> Result<Vec<T>> {
    let template = "{{json .}}";
    let mut args = vec![kind.to_string(), "list".to_string()];
    args.extend(session.map(str::to_string));
    args.extend(["--template".to_string(), template.to_string()]);
    let display = format!(
        "mutagen {kind} list{} --template '{template}'",
        session.map(|s| format!(" {s}")).unwrap_or_default()
    );
    let output = runner.run(display, "mutagen", &args, None)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "mutagen {kind} list failed: {}",
            stderr.lines().next().unwrap_or("no output")
        );
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

pub fn parse<T: DeserializeOwned>(json: &str) -> Result<Vec<T>> {
    let sessions: Option<Vec<T>> =
        serde_json::from_str(json.trim()).context("unreadable mutagen list output")?;
    Ok(sessions.unwrap_or_default())
}

/// Runs `mutagen <args>`, returns what it printed, and fails with its error line.
pub fn run(runner: &Runner, args: &[String]) -> Result<String> {
    let display = std::iter::once("mutagen".to_string())
        .chain(args.iter().map(|arg| {
            if arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:/=@~".contains(c))
            {
                arg.clone()
            } else {
                quote(arg)
            }
        }))
        .collect::<Vec<_>>()
        .join(" ");
    let output = runner.run(display, "mutagen", args, None)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .rev()
            .find(|l| l.contains("Error"))
            .or_else(|| stderr.lines().last())
            .unwrap_or("no output");
        bail!(
            "mutagen {}: {}",
            args[..2.min(args.len())].join(" "),
            line.trim()
        );
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/mutagen-sync.json");
    const FORWARDS: &str = include_str!("../tests/fixtures/mutagen-forward.json");

    #[test]
    fn reads_sessions_and_leases() {
        let sessions: Vec<SyncSession> = parse(SAMPLE).unwrap();
        let keys = vec!["crumb.lease".to_string(), "lymo-worktree".to_string()];
        let leases: Vec<_> = sessions.iter().filter_map(|s| s.lease(&keys)).collect();
        assert_eq!(leases, ["lym_1127", "lym_1136", "lym_1119"]);
        assert_eq!(sessions[0].successful_cycles, 649);
    }

    #[test]
    fn reads_alpha_path_and_conflicts() {
        let sessions: Vec<SyncSession> = parse(SAMPLE).unwrap();
        let orphan = sessions.iter().find(|s| s.name == "wt-lym-1127").unwrap();
        assert_eq!(orphan.local_alpha(), Some("/Users/dev/work/lym-1127"));
        assert_eq!(orphan.conflict_count(), 1);
    }

    #[test]
    fn reads_forwards() {
        let forwards: Vec<ForwardSession> = parse(FORWARDS).unwrap();
        let keys = vec!["crumb.lease".to_string()];
        assert_eq!(forwards[0].lease(&keys), Some("lym_1119"));
        assert_eq!(forwards[0].source.port(), Some(18107));
        assert_eq!(forwards[0].destination.port(), Some(8107));
        assert!(forwards[0].up());
    }

    #[test]
    fn empty_list_is_null() {
        assert!(parse::<SyncSession>("null\n").unwrap().is_empty());
    }
}
