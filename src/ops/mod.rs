//! What crumb changes: `up`, the lifecycle verbs and `reap`. Each step
//! ensures its piece exists, so rerunning after a failure continues instead
//! of duplicating. Every command goes through the runner and the command log.

mod up;
mod verbs;

pub mod reap;

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::checks::Images;
use crate::config::{Config, DbServer, Host, Ignore, Runtime};
use crate::lease::{Lease, PortForward};
use crate::mutagen;
use crate::run::{Runner, quote};
use crate::snapshot::{self, Facts};
use crate::template::Vars;
use crate::view;
use crate::wire;

pub use up::up;
pub use verbs::{down, drop, migrate, restart, stop, tunnel};

/// Where an operation reports what it is doing.
pub trait Progress: Sync {
    /// One step: `Syncing`, `wt-lym-1119 → indigo:~/wt/lym_1119`.
    fn step(&self, verb: &str, text: &str);
    /// A line of a command's own output, shown quietly.
    fn detail(&self, text: &str);
}

/// Everything an operation needs.
pub struct Ctx<'a> {
    pub config: &'a Config,
    pub runner: &'a Runner,
    pub out: &'a dyn Progress,
    /// The repository's main checkout, for seeds and fallbacks.
    pub main: Option<&'a Path>,
}

impl Ctx<'_> {
    fn host(&self) -> &Host {
        &self.config.host
    }

    /// Reads the current state and finds one lease in it.
    pub fn lease(&self, name: &str) -> Result<(Facts, Option<Lease>)> {
        let facts = snapshot::gather(self.config, self.runner)?;
        let snapshot = snapshot::assemble(self.config, &facts, &Images::default(), None);
        let lease = snapshot.leases.into_iter().find(|l| l.name == name);
        Ok((facts, lease))
    }

    fn require(&self, name: &str) -> Result<(Facts, Lease)> {
        match self.lease(name)? {
            (facts, Some(lease)) => Ok((facts, lease)),
            (_, None) => bail!("no lease named {name} on {}", self.config.host.label()),
        }
    }

    /// Runs a shell command on the host and fails with its last stderr line.
    fn host_command(&self, command: &str) -> Result<String> {
        let output = self.runner.command(self.host(), command)?;
        if !output.status.success() {
            bail!("{}", last_line(&output.stderr, "failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn last_line(bytes: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

/// Where crumb keeps a lease's compose file and labels on the host.
fn state_root(config: &Config) -> String {
    match config.host {
        Host::Local => "~/.local/state/crumb".to_string(),
        Host::Ssh(_) => format!("{}/.crumb", config.code_root.trim_end_matches('/')),
    }
}

/// The replica of a lease's worktree on the host.
fn replica(config: &Config, lease: &str) -> String {
    format!("{}/{lease}", config.code_root.trim_end_matches('/'))
}

/// A host path for a shell: `~/x` becomes `"$HOME"/'x'`.
fn shell_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None if path == "~" => "\"$HOME\"".to_string(),
        None => quote(path),
    }
}

/// A host path for a Mutagen URL, which takes paths relative to home.
fn mutagen_path(path: &str) -> &str {
    path.strip_prefix("~/").unwrap_or(path)
}

/// `--ignore` flags for the code sync.
fn ignore_flags(config: &Config, worktree: &Path, main: Option<&Path>) -> Result<Vec<String>> {
    let paths = match &config.ignore {
        Ignore::Paths(paths) => paths.clone(),
        Ignore::File(file) => {
            let path = [Some(worktree), main]
                .into_iter()
                .flatten()
                .map(|root| root.join(file))
                .find(|p| p.is_file())
                .with_context(|| {
                    format!("code.ignore names {file}, which is in neither this worktree nor the main checkout")
                })?;
            mutagen_ignore_paths(&std::fs::read_to_string(&path)?)
        }
    };
    Ok(paths.into_iter().map(|p| format!("--ignore={p}")).collect())
}

/// The `ignore.paths` list of a mutagen.yml, read line by line.
pub fn mutagen_ignore_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("paths:") && line.starts_with(char::is_whitespace) {
            inside = true;
            continue;
        }
        if !inside || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match trimmed.strip_prefix("- ") {
            Some(item) => {
                let item = item.trim().trim_matches(['"', '\'']);
                if !item.is_empty() {
                    paths.push(item.to_string());
                }
            }
            None => inside = false,
        }
    }
    paths
}

/// Waits until the sync has finished a cycle that started after now.
/// `mutagen sync flush` returns before a fresh replica's first scan lands, so
/// readiness is a new successful cycle while watching.
fn wait_sync(ctx: &Ctx, id: &str) -> Result<()> {
    let started = Instant::now();
    let before = mutagen::sync_session(ctx.runner, id)?
        .map(|s| s.successful_cycles)
        .unwrap_or(0);
    let _ = mutagen::run(
        ctx.runner,
        &[
            "sync".into(),
            "flush".into(),
            "--skip-wait".into(),
            id.into(),
        ],
    );
    let mut shown = String::new();
    loop {
        let Some(session) = mutagen::sync_session(ctx.runner, id)? else {
            bail!("the sync session {id} disappeared");
        };
        if session.status == "watching" && session.successful_cycles > before {
            ctx.out.step(
                "Synced",
                &format!(
                    "{} in {}",
                    session.name,
                    crate::run::duration(started.elapsed())
                ),
            );
            return Ok(());
        }
        let status = match &session.beta.staging_progress {
            Some(p) if p.expected_files > 0 => format!(
                "{} {}/{} files, {} MB",
                session.status,
                p.received_files / 500 * 500,
                p.expected_files,
                p.total_received_size / 10_000_000 * 10
            ),
            _ => session.status.clone(),
        };
        if status != shown {
            ctx.out.detail(&format!("sync: {status}"));
            shown = status;
        }
        if started.elapsed() > Duration::from_secs(600) {
            bail!(
                "{} did not finish a cycle in 10 minutes (status: {}{})",
                session.name,
                session.status,
                session
                    .last_error
                    .map(|e| format!(", {e}"))
                    .unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Makes the lease's port reachable on this machine through a Mutagen
/// forward, recreating one that points at the wrong ports.
fn ensure_forward(
    ctx: &Ctx,
    lease: &str,
    port: u16,
    existing: Option<&PortForward>,
) -> Result<u16> {
    let Host::Ssh(alias) = ctx.host() else {
        return Ok(port);
    };
    let local = ctx.config.local_port(port);
    if let Some(forward) = existing {
        if forward.local_port == Some(local) && forward.remote_port == Some(port) {
            if forward.paused {
                mutagen::run(
                    ctx.runner,
                    &["forward".into(), "resume".into(), forward.session.clone()],
                )?;
                ctx.out
                    .step("Resumed", &format!("forward :{local} → {alias}:{port}"));
            }
            return Ok(local);
        }
        mutagen::run(
            ctx.runner,
            &[
                "forward".into(),
                "terminate".into(),
                forward.session.clone(),
            ],
        )?;
    }
    mutagen::run(
        ctx.runner,
        &[
            "forward".into(),
            "create".into(),
            format!("--name={}", ctx.config.forward_name(lease)),
            format!("--label=crumb.lease={lease}"),
            format!("tcp:127.0.0.1:{local}"),
            format!("{alias}:tcp:127.0.0.1:{port}"),
        ],
    )?;
    ctx.out
        .step("Forwarded", &format!("127.0.0.1:{local} → {alias}:{port}"));
    Ok(local)
}

/// Writes every wired env file for the lease.
fn wire_all(ctx: &Ctx, lease: &str, worktree: &Path, vars: &Vars) -> Result<()> {
    for w in &ctx.config.wires {
        let missing: Vec<String> = w.set.values().flat_map(|v| vars.missing(v)).collect();
        if !missing.is_empty() {
            bail!(
                "{} uses {{{}}}, which has no value",
                w.file,
                missing.join("}, {")
            );
        }
        let written = wire::write(worktree, ctx.main, w, &wire::content(w, lease, vars))?;
        if written.seeded {
            ctx.out.detail(&format!(
                "copied {} from the main checkout",
                w.seed.as_deref().unwrap_or_default()
            ));
        }
        let note = if written.changed { "" } else { " (unchanged)" };
        ctx.out.step("Wired", &format!("{}{note}", w.file));
    }
    Ok(())
}

/// Removes the lease's wired files from its worktree.
fn unwire(ctx: &Ctx, worktree: &Path) -> Result<()> {
    for w in &ctx.config.wires {
        if wire::remove(worktree, w)? {
            ctx.out.step("Unwired", &w.file);
        }
    }
    Ok(())
}

/// Polls the service's container until it is healthy, or fails with the
/// error lines from its log as soon as it exits or restarts.
fn wait_healthy(ctx: &Ctx, project: &str, service: &str) -> Result<Duration> {
    let started = Instant::now();
    let command = format!(
        "id=$(docker ps -aq --filter label=com.docker.compose.project={p} --filter label=com.docker.compose.service={s} | head -n 1); \
         [ -z \"$id\" ] || docker inspect --format '{{{{.State.Status}}}} {{{{if .State.Health}}}}{{{{.State.Health.Status}}}}{{{{else}}}}none{{{{end}}}} {{{{.RestartCount}}}} {{{{.State.ExitCode}}}} {{{{.Name}}}}' \"$id\"",
        p = quote(project),
        s = quote(service),
    );
    let mut first_restarts = None;
    let mut running_since = None;
    let mut last_note = Instant::now();
    loop {
        let text = ctx.host_command(&command)?;
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [status, health, restarts, exit_code, name] = fields[..] else {
            bail!("no {service} container in {project}");
        };
        let name = name.trim_start_matches('/');
        let restarts: u64 = restarts.parse().unwrap_or(0);
        let first = *first_restarts.get_or_insert(restarts);
        let failed = match (status, health) {
            ("exited" | "dead", _) => Some(format!("{name} exited with code {exit_code}")),
            ("restarting", _) => Some(format!("{name} is restarting")),
            _ if restarts > first => Some(format!("{name} restarted")),
            (_, "unhealthy") => Some(format!("{name} is failing its health check")),
            _ => None,
        };
        if let Some(reason) = failed {
            let excerpt = log_excerpt(ctx, name);
            bail!(
                "{reason}{}",
                excerpt.map(|e| format!("\n{e}")).unwrap_or_default()
            );
        }
        match (status, health) {
            ("running", "healthy") => return Ok(started.elapsed()),
            ("running", "none") => {
                // Without a health check, a few seconds of running is the best sign.
                let since = *running_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(3) {
                    return Ok(started.elapsed());
                }
            }
            _ => {}
        }
        if last_note.elapsed() > Duration::from_secs(15) {
            ctx.out.detail(&format!(
                "{name}: {status}, health {health}, {}",
                crate::run::duration(started.elapsed())
            ));
            last_note = Instant::now();
        }
        if started.elapsed() > Duration::from_secs(900) {
            bail!("{name} was not healthy after 15 minutes (health: {health})");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The error lines at the end of a container's log, indented for output.
fn log_excerpt(ctx: &Ctx, container: &str) -> Option<String> {
    let output = ctx
        .runner
        .command(
            ctx.host(),
            &format!("docker logs --tail 300 {} 2>&1", quote(container)),
        )
        .ok()?;
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(view::clean_log_line)
        .collect();
    let excerpt = view::excerpt(&lines, 3);
    (!excerpt.is_empty()).then(|| {
        excerpt
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Runs a configured command on this machine per the command contract:
/// `CRUMB_*` variables in, progress on stderr, optionally one JSON object on
/// stdout whose fields come back as `{prefix.field}` variables.
fn run_command(
    ctx: &Ctx,
    what: &str,
    template: &str,
    vars: &Vars,
    cwd: &Path,
    prefix: &str,
) -> Result<Vars> {
    let missing = vars.missing(template);
    if !missing.is_empty() {
        bail!(
            "{what} uses {{{}}}, which has no value",
            missing.join("}, {")
        );
    }
    let command = vars.render(template);
    let output = ctx
        .runner
        .local(&command, cwd, &vars.env(), &|line| ctx.out.detail(line))?;
    if !output.status.success() {
        bail!(
            "{what} failed ({}): {}",
            output
                .status
                .code()
                .map_or("killed".to_string(), |c| format!("exit {c}")),
            last_line(&output.stderr, "no output")
        );
    }
    Ok(fields(&String::from_utf8_lossy(&output.stdout), prefix))
}

/// The fields of the last JSON object line in a command's stdout.
fn fields(stdout: &str, prefix: &str) -> Vars {
    let mut vars = Vars::default();
    let object = stdout.lines().rev().find_map(|line| {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(line.trim()).ok()
    });
    for (key, value) in object.into_iter().flatten() {
        let text = match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        vars.set(&format!("{prefix}.{key}"), text);
    }
    vars
}

/// Runs SQL on the lease database server, on the host.
fn psql(ctx: &Ctx, database: &str, sql: &str) -> Result<String> {
    let Some(db) = &ctx.config.database else {
        bail!("no [database] in the config");
    };
    let Some(server) = &db.server else {
        bail!("database.server is not set, so crumb cannot reach the database");
    };
    let psql = match server {
        DbServer::Docker(container) => format!(
            "docker exec -i {} psql -U {} -d {} -v ON_ERROR_STOP=1 -Atq",
            quote(container),
            quote(&db.user),
            quote(database)
        ),
        DbServer::Url(url) => format!(
            "psql {} -v ON_ERROR_STOP=1 -Atq",
            quote(&db_url(url, database))
        ),
    };
    let output = ctx.runner.script(
        ctx.host(),
        "psql.sql",
        &format!("{psql} <<'CRUMB_SQL'\n{sql}\nCRUMB_SQL\n"),
    )?;
    if !output.status.success() {
        bail!("{}", last_line(&output.stderr, "psql failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The server URL pointed at another database.
fn db_url(url: &str, database: &str) -> String {
    let (base, params) = match url.split_once('?') {
        Some((base, params)) => (base, format!("?{params}")),
        None => (url, String::new()),
    };
    let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
    let authority = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{authority}/{database}{params}")
}

/// A SQL string literal.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A SQL identifier.
fn sql_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn compose_only(config: &Config) -> Result<()> {
    match config.runtime {
        Runtime::Compose { .. } => Ok(()),
        Runtime::Process { .. } => bail!("this needs the compose runtime"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ignore_paths_from_mutagen_yml() {
        let text = "sync:\n  lamb:\n    alpha: \".\"\n  defaults:\n    ignore:\n      vcs: true\n      paths:\n\n        # Dependencies\n        - node_modules/\n        - \"*.tsbuildinfo\"\n    permissions:\n      x: 1\n";
        assert_eq!(
            mutagen_ignore_paths(text),
            ["node_modules/", "*.tsbuildinfo"]
        );
    }

    #[test]
    fn host_paths() {
        assert_eq!(shell_path("~/wt/a"), "\"$HOME\"/'wt/a'");
        assert_eq!(shell_path("/srv/a"), "'/srv/a'");
        assert_eq!(mutagen_path("~/wt/a"), "wt/a");
    }

    #[test]
    fn command_fields() {
        let vars = fields(
            "creating…\n{\"url\": \"postgres://x/y\", \"n\": 3}\n",
            "database",
        );
        assert_eq!(vars.get("database.url"), Some("postgres://x/y"));
        assert_eq!(vars.get("database.n"), Some("3"));
        assert_eq!(fields("no json", "database"), Vars::default());
    }

    #[test]
    fn database_urls() {
        assert_eq!(
            db_url("postgres://u:p@h:5432/postgres?sslmode=disable", "wt_a"),
            "postgres://u:p@h:5432/wt_a?sslmode=disable"
        );
        assert_eq!(db_url("postgresql://h", "wt_a"), "postgresql://h/wt_a");
    }
}
