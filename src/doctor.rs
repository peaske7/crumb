//! `crumb doctor`: checks each configured piece and says how to fix what
//! fails.

use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::agents;
use crate::config::{Config, DbServer, Forward, Host, Ignore, Runtime};
use crate::run::{Runner, duration, quote};

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// The command or change that fixes a failure.
    pub fix: Option<String>,
}

fn pass(name: impl Into<String>, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: true,
        detail: detail.into(),
        fix: None,
    }
}

fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: false,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

pub fn run(config: &Config, runner: &Runner, main: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let repo = config.repo.as_deref();
    checks.push(match repo {
        Some(repo) => pass(
            "config",
            repo.join(crate::config::PROJECT_FILE).display().to_string(),
        ),
        None => fail("config", "no crumb.toml here or above", "crumb init"),
    });

    let reachable = host(config, runner, &mut checks);
    if reachable {
        remote(config, runner, &mut checks);
    }
    if config.mutagen || config.forward == Forward::Mutagen {
        checks.push(
            match runner.run(
                "mutagen version".into(),
                "mutagen",
                &["version".into()],
                None,
            ) {
                Ok(out) if out.status.success() => pass(
                    "mutagen",
                    String::from_utf8_lossy(&out.stdout).trim().to_string(),
                ),
                _ => fail(
                    "mutagen",
                    "not installed",
                    "brew install mutagen-io/mutagen/mutagen",
                ),
            },
        );
    }
    if let Ignore::File(file) = &config.ignore
        && config.mutagen
    {
        let roots = [repo, main];
        let found = roots.iter().flatten().any(|root| root.join(file).is_file());
        checks.push(if found {
            pass("sync ignore", file.clone())
        } else {
            fail(
                "sync ignore",
                format!("{file} is in neither this worktree nor the main checkout"),
                format!("create {file}, or list paths in code.ignore"),
            )
        });
    }
    if let Runtime::Compose {
        file: Some(file), ..
    } = &config.runtime
    {
        let found = [repo, main]
            .iter()
            .flatten()
            .any(|root| root.join(file).is_file());
        checks.push(if found {
            pass("compose file", file.clone())
        } else {
            fail(
                "compose file",
                format!("{file} not found"),
                "set runtime.compose to the compose file's path",
            )
        });
    }
    let base = agents::base(runner, false);
    if let Ok(base) = base {
        let states = agents::status(&base, &agents::Client::ALL, false);
        let installed: Vec<&str> = states
            .iter()
            .filter(|s| s.state == agents::State::Current)
            .map(|s| s.client.label())
            .collect();
        let stale = states
            .iter()
            .any(|s| matches!(s.state, agents::State::Outdated | agents::State::Missing));
        checks.push(if stale {
            fail(
                "agent skill",
                "missing or older than this crumb",
                "crumb agents install",
            )
        } else if installed.is_empty() {
            pass("agent skill", "no agents on this machine")
        } else {
            pass("agent skill", installed.join(", "))
        });
    }
    checks
}

/// Whether the host answers, timed.
fn host(config: &Config, runner: &Runner, checks: &mut Vec<Check>) -> bool {
    let Host::Ssh(alias) = &config.host else {
        checks.push(pass("host", "this machine"));
        return true;
    };
    let started = Instant::now();
    match runner.command(&config.host, "true") {
        Ok(out) if out.status.success() => {
            checks.push(pass(format!("ssh {alias}"), duration(started.elapsed())));
            true
        }
        Ok(out) => {
            let why = String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("failed")
                .to_string();
            checks.push(fail(
                format!("ssh {alias}"),
                why,
                format!("check `ssh {alias}` works without a password (BatchMode)"),
            ));
            false
        }
        Err(err) => {
            checks.push(fail(
                format!("ssh {alias}"),
                format!("{err:#}"),
                "install OpenSSH",
            ));
            false
        }
    }
}

/// Everything checked on the host, in one round trip.
fn remote(config: &Config, runner: &Runner, checks: &mut Vec<Check>) {
    let mut script = String::from(
        "echo \"docker $(docker version --format '{{.Server.Version}}' 2>/dev/null)\"\n\
         echo \"compose $(docker compose version --short 2>/dev/null)\"\n\
         echo \"flock $(command -v flock)\"\n\
         echo \"tmux $(command -v tmux)\"\n",
    );
    if let Runtime::Compose {
        env_file: Some(file),
        ..
    } = &config.runtime
    {
        script.push_str(&format!(
            "f={}; case \"$f\" in \"~/\"*) f=\"$HOME/${{f#\\~/}}\";; esac; [ -f \"$f\" ] && echo \"envfile ok\" || echo \"envfile\"\n",
            quote(file)
        ));
    }
    if let Some(db) = &config.database
        && let Some(server) = &db.server
    {
        let psql = match server {
            DbServer::Docker(c) => format!(
                "docker exec {} psql -U {} -d postgres -Atc 'select 1'",
                quote(c),
                quote(&db.user)
            ),
            DbServer::Url(url) => format!("psql {} -Atc 'select 1'", quote(url)),
        };
        script.push_str(&format!("echo \"database $({psql} 2>/dev/null)\"\n"));
    }
    let output = match runner.script(&config.host, "doctor.sh", &script) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(err) => {
            checks.push(fail("host checks", format!("{err:#}"), "rerun with -v"));
            return;
        }
    };
    let value = |key: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let host = config.host.label();
    match &config.runtime {
        Runtime::Compose { env_file, .. } => {
            let docker = value("docker");
            checks.push(if docker.is_empty() {
                fail(
                    "docker",
                    format!("not reachable on {host}"),
                    "install Docker and add your user to the docker group",
                )
            } else {
                pass("docker", docker)
            });
            let compose = value("compose");
            checks.push(if compose.is_empty() {
                fail(
                    "compose",
                    "the compose plugin is missing",
                    "install docker-compose-plugin",
                )
            } else {
                pass("compose", compose)
            });
            if let Some(file) = env_file {
                checks.push(if value("envfile") == "ok" {
                    pass("env file", file.clone())
                } else {
                    fail(
                        "env file",
                        format!("{file} is missing on {host}"),
                        format!("create {file} on {host}"),
                    )
                });
            }
        }
        Runtime::Process { .. } => {
            checks.push(if value("tmux").is_empty() {
                fail("tmux", format!("not installed on {host}"), "install tmux")
            } else {
                pass("tmux", value("tmux"))
            });
        }
    }
    if !config.host.is_local() {
        checks.push(if value("flock").is_empty() {
            fail(
                "flock",
                "missing, so crumb falls back to a lock directory",
                "install util-linux",
            )
        } else {
            pass("flock", value("flock"))
        });
    }
    if let Some(db) = &config.database
        && db.server.is_some()
    {
        checks.push(if value("database") == "1" {
            pass("database", "the server answers")
        } else {
            fail(
                "database",
                "the server did not answer `select 1`",
                "check database.server and that the container is running",
            )
        });
    }
}

pub fn print(checks: &[Check], color: bool) {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let paint = |text: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    for check in checks {
        let mark = if check.ok {
            paint("✓", "32")
        } else {
            paint("✗", "31")
        };
        let fix = check
            .fix
            .as_ref()
            .filter(|_| !check.ok)
            .map(|fix| format!("  → {fix}"))
            .unwrap_or_default();
        println!(
            "{mark} {:<width$}  {}{}",
            check.name,
            paint(&check.detail, "2"),
            fix
        );
    }
}
