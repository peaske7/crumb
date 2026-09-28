use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::run::Runner;

/// One Mutagen synchronization session, as `mutagen sync list --template` prints it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSession {
    pub name: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
    #[serde(default)]
    pub paused: bool,
    pub status: String,
    #[serde(default)]
    pub last_error: Option<String>,
    pub alpha: Endpoint,
    pub beta: Endpoint,
    #[serde(default)]
    pub conflicts: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub protocol: String,
    pub path: String,
    #[serde(default)]
    pub connected: bool,
}

impl SyncSession {
    /// The lease this session belongs to, from the first matching label key.
    pub fn lease(&self, label_keys: &[String]) -> Option<&str> {
        let labels = self.labels.as_ref()?;
        label_keys
            .iter()
            .find_map(|key| labels.get(key))
            .map(String::as_str)
    }

    /// The worktree path when the alpha side is on this machine.
    pub fn local_alpha(&self) -> Option<&str> {
        (self.alpha.protocol == "local").then_some(self.alpha.path.as_str())
    }

    pub fn conflict_count(&self) -> usize {
        self.conflicts.as_ref().map_or(0, Vec::len)
    }
}

pub fn list(runner: &Runner) -> Result<Vec<SyncSession>> {
    let template = "{{json .}}";
    let output = runner.run(
        format!("mutagen sync list --template '{template}'"),
        "mutagen",
        &[
            "sync".to_string(),
            "list".to_string(),
            "--template".to_string(),
            template.to_string(),
        ],
        None,
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "mutagen sync list failed: {}",
            stderr.lines().next().unwrap_or("no output")
        );
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

pub fn parse(json: &str) -> Result<Vec<SyncSession>> {
    let sessions: Option<Vec<SyncSession>> =
        serde_json::from_str(json.trim()).context("unreadable mutagen sync list output")?;
    Ok(sessions.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/mutagen-sync.json");

    #[test]
    fn reads_sessions_and_leases() {
        let sessions = parse(SAMPLE).unwrap();
        let keys = vec!["crumb.lease".to_string(), "lymo-worktree".to_string()];
        let leases: Vec<_> = sessions.iter().filter_map(|s| s.lease(&keys)).collect();
        assert_eq!(leases, ["lym_1127", "lym_1136", "lym_1119"]);
    }

    #[test]
    fn reads_alpha_path_and_conflicts() {
        let sessions = parse(SAMPLE).unwrap();
        let orphan = sessions.iter().find(|s| s.name == "wt-lym-1127").unwrap();
        assert_eq!(orphan.local_alpha(), Some("/Users/dev/work/lym-1127"));
        assert_eq!(orphan.conflict_count(), 1);
    }

    #[test]
    fn empty_list_is_null() {
        assert!(parse("null\n").unwrap().is_empty());
    }
}
