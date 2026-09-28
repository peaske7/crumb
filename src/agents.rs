//! `crumb agents`: installs the crumb skill for coding agents. The skill is
//! compiled into the binary, so `status` can say when an installed copy
//! drifted from this version.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const SKILL: &str = include_str!("../skills/crumb/SKILL.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Client {
    ClaudeCode,
    Codex,
}

impl Client {
    pub const ALL: [Client; 2] = [Client::ClaudeCode, Client::Codex];

    pub fn parse(value: &str) -> Result<Vec<Client>> {
        match value {
            "all" => Ok(Client::ALL.to_vec()),
            "claude-code" | "claude" => Ok(vec![Client::ClaudeCode]),
            "codex" => Ok(vec![Client::Codex]),
            other => bail!("--client is claude-code, codex or all, not {other:?}"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude-code",
            Client::Codex => "codex",
        }
    }

    /// The client's own directory: `~/.claude`, or `.claude` in a project.
    fn root(self, base: &Path) -> PathBuf {
        base.join(match self {
            Client::ClaudeCode => ".claude",
            Client::Codex => ".codex",
        })
    }

    pub fn skill_path(self, base: &Path) -> PathBuf {
        self.root(base)
            .join("skills")
            .join("crumb")
            .join("SKILL.md")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Current,
    /// Installed, but not the text this binary carries.
    Outdated,
    Missing,
    /// The client isn't set up on this machine, so the skill was skipped.
    NoClient,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub client: Client,
    pub path: PathBuf,
    pub state: State,
}

/// Where skills go: the home directory, or the repository with `--project`.
pub fn base(runner: &crate::run::Runner, project: bool) -> Result<PathBuf> {
    if project {
        let cwd = std::env::current_dir()?;
        return crate::worktree::find(runner, &cwd)
            .map(|c| c.root)
            .context("--project needs a git repository");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn status(base: &Path, clients: &[Client], project: bool) -> Vec<Status> {
    clients
        .iter()
        .map(|&client| {
            let path = client.skill_path(base);
            let state = match std::fs::read_to_string(&path) {
                Ok(text) if text == SKILL => State::Current,
                Ok(_) => State::Outdated,
                // A project install doesn't need the client's directory yet.
                Err(_) if !project && !client.root(base).is_dir() => State::NoClient,
                Err(_) => State::Missing,
            };
            Status {
                client,
                path,
                state,
            }
        })
        .collect()
}

/// Writes the skill for each client that is set up (or every one with
/// `--project`). Returns what it did.
pub fn install(base: &Path, clients: &[Client], project: bool) -> Result<Vec<Status>> {
    let mut done = Vec::new();
    for status in status(base, clients, project) {
        if status.state == State::NoClient {
            done.push(status);
            continue;
        }
        if status.state != State::Current {
            let dir = status.path.parent().expect("skill path has a parent");
            std::fs::create_dir_all(dir)?;
            std::fs::write(&status.path, SKILL)
                .with_context(|| format!("writing {}", status.path.display()))?;
        }
        done.push(Status {
            state: State::Current,
            ..status
        });
    }
    Ok(done)
}

/// Removes the skill directory crumb wrote. Returns the paths removed.
pub fn uninstall(base: &Path, clients: &[Client]) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for client in clients {
        let path = client.skill_path(base);
        if path.is_file() {
            let dir = path.parent().expect("skill path has a parent");
            std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
            removed.push(path);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("crumb-agents-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn skips_clients_that_are_not_set_up() {
        let home = Scratch::new("skip");
        std::fs::create_dir_all(home.0.join(".claude")).unwrap();
        let done = install(&home.0, &Client::ALL, false).unwrap();
        assert_eq!(done[0].state, State::Current);
        assert_eq!(done[1].state, State::NoClient);
        assert!(home.0.join(".claude/skills/crumb/SKILL.md").is_file());
        assert!(!home.0.join(".codex").exists());
    }

    #[test]
    fn reports_drift_and_uninstalls() {
        let home = Scratch::new("drift");
        std::fs::create_dir_all(home.0.join(".codex")).unwrap();
        install(&home.0, &[Client::Codex], false).unwrap();
        let path = Client::Codex.skill_path(&home.0);
        std::fs::write(&path, "old").unwrap();
        assert_eq!(
            status(&home.0, &[Client::Codex], false)[0].state,
            State::Outdated
        );
        assert_eq!(uninstall(&home.0, &[Client::Codex]).unwrap(), [path]);
        assert_eq!(
            status(&home.0, &[Client::Codex], false)[0].state,
            State::Missing
        );
    }

    #[test]
    fn the_skill_has_frontmatter() {
        assert!(SKILL.starts_with("---\nname: crumb\ndescription: "));
    }
}
