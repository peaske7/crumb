//! `crumb up`: database, sync, port and runtime, forward, wiring, health.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use super::{
    Ctx, ensure_forward, ignore_flags, last_line, mutagen_path, psql, replica, run_command,
    sql_ident, sql_literal, state_root, wait_ready, wait_sync, wire_all,
};
use crate::config::{DbServer, Forward, Host, Runtime};
use crate::lease::Lease;
use crate::mutagen;
use crate::run::{duration, quote};
use crate::snapshot::Facts;
use crate::template::Vars;
use crate::worktree::check_name;

const SCRIPT: &str = include_str!("up.sh");

pub fn up(ctx: &Ctx, lease: &str, worktree: &Path) -> Result<()> {
    let started = Instant::now();
    check_name(lease)?;
    if !worktree.is_dir() {
        bail!("{} is not a directory", worktree.display());
    }
    let (facts, existing) = ctx.lease(lease)?;
    if let Some(other) = existing.as_ref().and_then(Lease::live_worktree)
        && Path::new(other) != worktree
    {
        bail!("{lease} belongs to {other}; give this worktree another name with --name");
    }

    let mut vars = Vars::default();
    vars.set("lease", lease)
        .set("host", ctx.config.host.label())
        .set("worktree", worktree.to_string_lossy());

    let database = ensure_database(ctx, &facts, lease, worktree, &mut vars)?;
    if ctx.config.mutagen {
        ensure_sync(ctx, &facts, lease, worktree)?;
    }
    let port = start(ctx, &facts, lease, worktree, database.as_deref(), &vars)?;
    vars.set("port", port.to_string());
    if let Some(nn) = port
        .checked_sub(ctx.config.host_base)
        .filter(|nn| *nn < 100)
    {
        vars.set("n", format!("{nn:02}"));
    }
    let local_port = match ctx.config.forward {
        Forward::Mutagen => ensure_forward(
            ctx,
            lease,
            port,
            existing.as_ref().and_then(|l| l.forward.as_ref()),
        )?,
        Forward::Direct => port,
    };
    vars.set("local_port", local_port.to_string());
    wire_all(ctx, lease, worktree, &vars)?;
    wait_ready(ctx, lease, &vars)?;
    let url = format!("http://127.0.0.1:{local_port}");
    ctx.out.step(
        "Ready",
        &format!("{lease} at {url} in {}", duration(started.elapsed())),
    );
    Ok(())
}

/// Creates the lease database unless the server already lists it, and labels
/// it with where it came from. Refuses when another backend uses it.
fn ensure_database(
    ctx: &Ctx,
    facts: &Facts,
    lease: &str,
    worktree: &Path,
    vars: &mut Vars,
) -> Result<Option<String>> {
    let Some(db) = &ctx.config.database else {
        return Ok(None);
    };
    let name = db.name.render(lease);
    vars.set("database", name.as_str());
    let project = ctx.config.project.render(lease);
    if let Some(other) = facts.host.containers.iter().find(|c| {
        c.status == "running"
            && c.label("crumb.db") == Some(name.as_str())
            && c.label("com.docker.compose.project") != Some(project.as_str())
    }) {
        bail!(
            "{} already runs on {name}; two backends on one database take each other's jobs",
            other.name.trim_start_matches('/')
        );
    }
    let listed = facts.host.databases.iter().any(|d| d.name == name);
    if listed {
        ctx.out.step("Database", &format!("{name} (exists)"));
    } else if let Some(create) = &db.create {
        ctx.out.step("Creating", &format!("database {name}"));
        let fields = run_command(ctx, "database.create", create, vars, worktree, "database")?;
        vars.extend(fields);
    } else if let Some(from) = &db.from {
        ctx.out
            .step("Copying", &format!("database {from} to {name}"));
        copy_database(ctx, from, &name)?;
    } else if db.server.is_some() {
        ctx.out.step("Creating", &format!("database {name}"));
        psql(
            ctx,
            "postgres",
            &format!("create database {};", sql_ident(&name)),
        )?;
    }
    if db.server.is_some() {
        let comment = serde_json::json!({
            "crumb": 1,
            "lease": lease,
            "worktree": worktree.to_string_lossy(),
            "from": db.from,
            "at": jiff::Timestamp::now().to_string(),
        })
        .to_string();
        psql(
            ctx,
            "postgres",
            &format!(
                "do $$ begin\n  if shobj_description((select oid from pg_database where datname = {n}), 'pg_database') is null then\n    execute format('comment on database %I is %L', {n}, {c});\n  end if;\nend $$;",
                n = sql_literal(&name),
                c = sql_literal(&comment),
            ),
        )
        .context("labelling the database")?;
    }
    Ok(Some(name))
}

/// Copies `from` into a new database with pg_dump. `CREATE DATABASE …
/// TEMPLATE` would fail whenever anything is connected to the source.
fn copy_database(ctx: &Ctx, from: &str, name: &str) -> Result<()> {
    let Some(db) = &ctx.config.database else {
        return Ok(());
    };
    let Some(server) = &db.server else {
        bail!("database.from needs database.server");
    };
    psql(
        ctx,
        "postgres",
        &format!("create database {};", sql_ident(name)),
    )?;
    let pipe = match server {
        DbServer::Docker(container) => format!(
            "docker exec {c} pg_dump -U {u} -d {f} | docker exec -i {c} psql -U {u} -d {n} -q",
            c = quote(container),
            u = quote(&db.user),
            f = quote(from),
            n = quote(name),
        ),
        DbServer::Url(url) => format!(
            "pg_dump {} | psql {} -q",
            quote(&super::db_url(url, from)),
            quote(&super::db_url(url, name)),
        ),
    };
    // A restore reports errors for objects the target already has; count
    // them rather than stop on the first.
    let output = ctx.runner.command(
        &ctx.config.host,
        &format!("{pipe} 2>&1 | grep -c ERROR || true"),
    )?;
    let errors = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if errors != "0" && !errors.is_empty() {
        ctx.out
            .detail(&format!("the restore reported {errors} errors"));
    }
    Ok(())
}

/// Creates or resumes the lease's one-way replica, then waits for a full cycle.
fn ensure_sync(ctx: &Ctx, facts: &Facts, lease: &str, worktree: &Path) -> Result<()> {
    let Host::Ssh(alias) = &ctx.config.host else {
        return Ok(());
    };
    let existing = facts
        .syncs
        .iter()
        .find(|s| s.lease(&ctx.config.label_keys) == Some(lease));
    let id = match existing {
        Some(session) => {
            if let Some(alpha) = session.local_alpha()
                && Path::new(alpha) != worktree
            {
                bail!(
                    "the sync session {} copies {alpha}; `crumb down {lease}` first",
                    session.name
                );
            }
            if session.paused {
                mutagen::run(
                    ctx.runner,
                    &["sync".into(), "resume".into(), session.identifier.clone()],
                )?;
                ctx.out.step("Resumed", &format!("sync {}", session.name));
            }
            session.identifier.clone()
        }
        None => {
            let beta = replica(ctx.config, lease);
            ctx.out.step(
                "Syncing",
                &format!("{} → {alias}:{beta}", worktree.display()),
            );
            let mut args: Vec<String> = [
                "sync",
                "create",
                "--mode=one-way-replica",
                "--ignore-vcs",
                "--default-file-mode=0644",
                "--default-directory-mode=0755",
            ]
            .map(String::from)
            .to_vec();
            args.push(format!("--name={}", ctx.config.sync_name(lease)));
            args.push(format!("--label=crumb.lease={lease}"));
            args.extend(ignore_flags(ctx.config, worktree, ctx.main)?);
            args.push(worktree.to_string_lossy().into_owned());
            args.push(format!("{alias}:{}", mutagen_path(&beta)));
            let printed = mutagen::run(ctx.runner, &args)?;
            printed
                .split_whitespace()
                .find(|word| word.starts_with("sync_"))
                .map(str::to_string)
                .unwrap_or_else(|| ctx.config.sync_name(lease))
        }
    };
    wait_sync(ctx, &id)
}

/// Picks the port and starts the runtime on the host, under the host's lock.
fn start(
    ctx: &Ctx,
    facts: &Facts,
    lease: &str,
    worktree: &Path,
    database: Option<&str>,
    vars: &Vars,
) -> Result<u16> {
    let config = ctx.config;
    let project = config.project.render(lease);
    let remote = !config.host.is_local();
    let project_dir = if remote {
        replica(config, lease)
    } else {
        worktree.to_string_lossy().into_owned()
    };
    let mut header = vec![
        ("LEASE", lease.to_string()),
        ("PROJECT", project.clone()),
        ("STATE_ROOT", state_root(config)),
        ("PROJECT_DIR", project_dir),
        ("DATABASE", database.unwrap_or_default().to_string()),
        ("WORKTREE", worktree.to_string_lossy().into_owned()),
        ("HOST_BASE", config.host_base.to_string()),
        ("LOCAL_BASE", config.local_base.to_string()),
        ("LOCAL_USED", local_ports_in_use(ctx, facts).join(",")),
        ("MEMORY_MB", opt(config.memory_mb)),
        ("KEEP_FREE_MB", opt(config.keep_free_mb)),
    ];
    let mut functions = String::new();
    match &config.runtime {
        Runtime::Compose {
            file,
            env_file,
            up_args,
            subnet,
        } => {
            let service = config
                .service
                .as_deref()
                .context("runtime.service names the service crumb waits for; set it")?;
            let file = file
                .as_deref()
                .context("runtime.compose names the compose file; set it")?;
            let source = find_file(worktree, ctx.main, file)?;
            let compose_file = if remote {
                let text = std::fs::read_to_string(&source)
                    .with_context(|| format!("reading {}", source.display()))?;
                functions.push_str(&heredoc("write_compose", "CRUMB_COMPOSE_EOF", &text)?);
                String::new()
            } else {
                source.to_string_lossy().into_owned()
            };
            functions.push_str(&heredoc(
                "write_labels",
                "CRUMB_LABELS_EOF",
                &labels(service, lease, database, worktree, subnet.is_some()),
            )?);
            header.extend([
                ("RUNTIME", "compose".to_string()),
                ("SUBNET", subnet.clone().unwrap_or_default()),
                ("COMPOSE_FILE", compose_file),
                ("ENV_FILE", env_file.clone().unwrap_or_default()),
                ("SERVICE", service.to_string()),
                ("UP_ARGS", up_args.join(" ")),
                ("START", String::new()),
            ]);
        }
        Runtime::Process { start, .. } => {
            header.extend([
                ("RUNTIME", "process".to_string()),
                ("SUBNET", String::new()),
                ("COMPOSE_FILE", String::new()),
                ("ENV_FILE", String::new()),
                ("SERVICE", String::new()),
                ("UP_ARGS", String::new()),
                ("START", vars.render(start)),
            ]);
        }
    }
    let mut script: String = header
        .iter()
        .map(|(key, value)| format!("{key}={}\n", quote(value)))
        .collect();
    script.push_str(&functions);
    script.push_str(SCRIPT);

    ctx.out
        .step("Starting", &format!("{project} on {}", config.host.label()));
    // Compose prints each progress line twice when it thinks it has a terminal.
    let last = std::sync::Mutex::new(String::new());
    let output = ctx
        .runner
        .script_streaming(&config.host, "up.sh", &script, &|line| {
            let line = line.trim();
            let mut last = last.lock().expect("last line lock");
            if !line.is_empty() && *last != line {
                ctx.out.detail(line);
                *last = line.to_string();
            }
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let port = stdout
        .lines()
        .find_map(|line| line.strip_prefix("@@port "))
        .and_then(|p| p.trim().parse().ok());
    if !output.status.success() {
        bail!(
            "{}",
            last_line(&output.stderr, "starting the runtime failed")
        );
    }
    port.context("the host script did not report a port")
}

fn opt(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// A shell function that prints `text`, with `marker` as the heredoc end.
fn heredoc(name: &str, marker: &str, text: &str) -> Result<String> {
    if text.lines().any(|line| line == marker) {
        bail!("the text contains the line {marker}");
    }
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    Ok(format!(
        "{name}() {{\ncat <<'{marker}'\n{text}{newline}{marker}\n}}\n"
    ))
}

/// The compose override that labels the service with the lease, and pins the
/// default network's subnet when the project sets one. `@@PORT@@` and
/// `@@SUBNET@@` are filled in on the host once the port is chosen.
fn labels(
    service: &str,
    lease: &str,
    database: Option<&str>,
    worktree: &Path,
    subnet: bool,
) -> String {
    let json = |value: &str| serde_json::Value::String(value.to_string()).to_string();
    let mut text = format!(
        "services:\n  {service}:\n    labels:\n      crumb.lease: {}\n      crumb.port: \"@@PORT@@\"\n      crumb.worktree: {}\n",
        json(lease),
        json(&worktree.to_string_lossy()),
    );
    if let Some(database) = database {
        text.push_str(&format!("      crumb.db: {}\n", json(database)));
    }
    if subnet {
        text.push_str(
            "networks:\n  default:\n    ipam:\n      config:\n        - subnet: \"@@SUBNET@@\"\n",
        );
    }
    text
}

/// The file in the worktree, else in the main checkout (for branches that
/// predate it).
fn find_file(worktree: &Path, main: Option<&Path>, file: &str) -> Result<PathBuf> {
    [Some(worktree), main]
        .into_iter()
        .flatten()
        .map(|root| root.join(file))
        .find(|p| p.is_file())
        .with_context(|| format!("{file} is in neither this worktree nor the main checkout"))
}

/// Local ports a new lease must not take: listeners here and forwards.
fn local_ports_in_use(ctx: &Ctx, facts: &Facts) -> Vec<String> {
    if ctx.config.forward != Forward::Mutagen {
        return Vec::new();
    }
    let mut ports: Vec<u16> = facts
        .forwards
        .iter()
        .filter_map(|f| f.source.port())
        .collect();
    if let Ok(output) = ctx.runner.run(
        "lsof -nP -iTCP -sTCP:LISTEN".to_string(),
        "lsof",
        &["-nP".into(), "-iTCP".into(), "-sTCP:LISTEN".into()],
        None,
    ) {
        ports.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().nth(8))
                .filter_map(|addr| addr.rsplit(':').next()?.parse::<u16>().ok()),
        );
    }
    let low = ctx.config.local_base;
    ports.retain(|p| *p > low && *p < low.saturating_add(100));
    ports.sort_unstable();
    ports.dedup();
    ports.iter().map(u16::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_valid_yaml_strings() {
        let text = labels(
            "backend",
            "a",
            Some("wt_a"),
            Path::new("/w/it's \"here\""),
            false,
        );
        assert!(text.contains("crumb.worktree: \"/w/it's \\\"here\\\"\""));
        assert!(text.contains("crumb.port: \"@@PORT@@\""));
        assert!(!text.contains("networks:"));
    }

    #[test]
    fn a_subnet_pins_the_default_network() {
        let text = labels("backend", "a", None, Path::new("/w"), true);
        assert!(text.ends_with(
            "networks:\n  default:\n    ipam:\n      config:\n        - subnet: \"@@SUBNET@@\"\n"
        ));
    }

    #[test]
    fn refuses_a_database_another_backend_runs_on() {
        struct Quiet;
        impl crate::ops::Progress for Quiet {
            fn step(&self, _: &str, _: &str) {}
            fn detail(&self, _: &str) {}
        }
        let config = crate::config::Config::parse(
            "[runtime]\nproject = \"wt_{lease}\"\n[database]\nname = \"wt_{lease}\"\nserver = \"docker://db\"",
        )
        .unwrap();
        let runner = crate::run::Runner::default();
        let ctx = Ctx {
            config: &config,
            runner: &runner,
            out: &Quiet,
            main: None,
        };
        let mut host = crate::probe::parse(include_str!("../../tests/fixtures/probe.txt")).unwrap();
        let other = host
            .containers
            .iter_mut()
            .find(|c| c.name == "/wt_lym_1119-backend-1")
            .unwrap();
        other
            .labels
            .get_or_insert_default()
            .insert("crumb.db".into(), "wt_a".into());
        let facts = Facts {
            at: jiff::Timestamp::now(),
            host,
            syncs: vec![],
            forwards: vec![],
            warnings: vec![],
        };
        let err = ensure_database(&ctx, &facts, "a", Path::new("/w"), &mut Vars::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("already runs on wt_a"), "{err}");
        assert!(runner.records().is_empty(), "no command may run first");
    }

    #[test]
    fn heredocs_refuse_their_own_marker() {
        assert!(heredoc("f", "EOF", "a\nEOF\n").is_err());
        assert_eq!(
            heredoc("f", "EOF", "a").unwrap(),
            "f() {\ncat <<'EOF'\na\nEOF\n}\n"
        );
    }
}
