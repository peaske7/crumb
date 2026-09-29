//! `crumb doctor`: checks each configured piece and says how to fix what
//! fails.

use std::path::{Path, PathBuf};
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

/// `worktree` is the checkout crumb runs in, `main` the repository's main
/// checkout.
pub fn run(
    config: &Config,
    runner: &Runner,
    worktree: Option<&Path>,
    main: Option<&Path>,
) -> Vec<Check> {
    let mut checks = vec![pass("crumb", crate::VERSION)];
    let repo = config.repo.as_deref();
    // Where project files are looked up: this worktree first.
    let roots: Vec<&Path> = [worktree, repo, main].into_iter().flatten().collect();
    checks.push(match &config.source {
        Some(source) => pass("config", source.label()),
        None => fail(
            "config",
            "no crumb.toml in this worktree, its main checkout or the default branch",
            "crumb init",
        ),
    });

    let reachable = host(config, runner, &mut checks);
    if reachable {
        // The source database is measured against the main checkout, which
        // is usually on the default branch.
        let migrations = config.schema.as_ref().and_then(|schema| {
            [main, repo]
                .into_iter()
                .flatten()
                .map(|root| root.join(&schema.migrations))
                .find(|dir| dir.is_dir())
        });
        remote(config, runner, migrations, &mut checks);
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
        let found = roots.iter().any(|root| root.join(file).is_file());
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
        let found = roots
            .iter()
            .map(|root| root.join(file))
            .find(|p| p.is_file());
        checks.push(match found {
            None => fail(
                "compose file",
                format!("{file} not found"),
                "set runtime.compose to the compose file's path",
            ),
            Some(path) => compose_file(file, &path, worktree, main),
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

/// Whether the compose file can bind the lease's port.
fn compose_file(file: &str, path: &Path, worktree: Option<&Path>, main: Option<&Path>) -> Check {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let here = worktree.is_some_and(|w| path.starts_with(w));
    let place = match main {
        Some(main) if !here && path.starts_with(main) => " (main checkout)",
        _ => "",
    };
    if text.contains("CRUMB_PORT") {
        return pass("compose file", format!("{file}{place}"));
    }
    let fix = if here && worktree != main {
        "merge the default branch; this branch predates crumb"
    } else {
        "publish the service as 127.0.0.1:${CRUMB_PORT}:<port>"
    };
    fail(
        "compose file",
        format!("{file}{place} doesn't use ${{CRUMB_PORT}}"),
        fix,
    )
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
fn remote(config: &Config, runner: &Runner, migrations: Option<PathBuf>, checks: &mut Vec<Check>) {
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
    let source = source_query(config);
    if let Some(query) = &source {
        script.push_str(&format!(
            "echo \"source $({query} 2>/dev/null | head -n 1)\"\n"
        ));
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
    if source.is_some()
        && let (Some(dir), Some(schema), Some(from)) = (
            migrations,
            &config.schema,
            config.database.as_ref().and_then(|db| db.from.as_deref()),
        )
    {
        checks.push(source_schema(
            from,
            &value("source"),
            &dir,
            &schema.migrations,
        ));
    }
}

/// The command that reads the newest applied migration of `database.from`,
/// when there is one to read.
fn source_query(config: &Config) -> Option<String> {
    let schema = config.schema.as_ref()?;
    let db = config.database.as_ref()?;
    let from = db.from.as_deref()?;
    Some(match db.server.as_ref()? {
        DbServer::Docker(c) => format!(
            "docker exec {} psql -U {} -d {} -Atc {}",
            quote(c),
            quote(&db.user),
            quote(from),
            quote(&schema.query)
        ),
        DbServer::Url(url) => format!(
            "psql {} -Atc {}",
            quote(&crate::ops::db_url(url, from)),
            quote(&schema.query)
        ),
    })
}

/// How far `database.from` is behind the migrations. Every lease copies it,
/// so a source that lags starts each new lease behind.
fn source_schema(from: &str, applied: &str, migrations: &Path, dir: &str) -> Check {
    if applied.is_empty() {
        return fail(
            "source schema",
            format!("could not read the applied migration of {from}"),
            "check that checks.schema.query runs in database.from",
        );
    }
    match crate::checks::migrations_after(migrations, applied) {
        0 => pass("source schema", format!("{from} is current at {applied}")),
        n => fail(
            "source schema",
            format!(
                "{from} is {n} migration{} behind {dir}; every new lease starts behind",
                if n == 1 { "" } else { "s" }
            ),
            format!("apply the migrations to {from}, or set database.migrate_on_up = true"),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_compose_file_without_crumb_port_predates_crumb() {
        let dir = std::env::temp_dir().join(format!("crumb-doctor-{}", std::process::id()));
        let (main, worktree) = (dir.join("main"), dir.join("wt"));
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join("compose.yml"),
            "ports: [\"${OLD_PORT:?}:80\"]\n",
        )
        .unwrap();
        std::fs::write(main.join("compose.yml"), "ports: [\"${CRUMB_PORT}:80\"]\n").unwrap();
        let old = compose_file(
            "compose.yml",
            &worktree.join("compose.yml"),
            Some(&worktree),
            Some(&main),
        );
        let current = compose_file(
            "compose.yml",
            &main.join("compose.yml"),
            Some(&worktree),
            Some(&main),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!old.ok);
        assert_eq!(old.detail, "compose.yml doesn't use ${CRUMB_PORT}");
        assert_eq!(
            old.fix.as_deref(),
            Some("merge the default branch; this branch predates crumb")
        );
        assert!(current.ok);
        assert_eq!(current.detail, "compose.yml (main checkout)");
    }

    #[test]
    fn a_source_database_behind_the_migrations_fails() {
        let dir = std::env::temp_dir().join(format!("crumb-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["001_a.sql", "002_b.sql", "003_c.sql"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let behind = source_schema("app_dev", "001", &dir, "db/migrations");
        let current = source_schema("app_dev", "003", &dir, "db/migrations");
        let unknown = source_schema("app_dev", "", &dir, "db/migrations");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!behind.ok);
        assert_eq!(
            behind.detail,
            "app_dev is 2 migrations behind db/migrations; every new lease starts behind"
        );
        assert!(current.ok);
        assert!(!unknown.ok);
    }
}
