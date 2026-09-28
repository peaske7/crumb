//! restart, stop, down, drop, migrate and tunnel for an existing lease.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{
    Ctx, ensure_forward, psql, replica, run_command, shell_path, sql_ident, state_root, unwire,
    wait_healthy, wait_ready, wait_sync, wire_all,
};
use crate::config::{Forward, Host, Runtime};
use crate::lease::Lease;
use crate::mutagen;
use crate::run::{duration, quote};
use crate::worktree::check_name;

/// Waits for the sync to land, restarts the backend and waits for health.
pub fn restart(ctx: &Ctx, name: &str) -> Result<()> {
    let (_, lease) = ctx.require(name)?;
    if let Some(sync) = &lease.sync {
        if sync.paused {
            mutagen::run(
                ctx.runner,
                &["sync".into(), "resume".into(), sync.session.clone()],
            )?;
        }
        wait_sync(ctx, &sync.session)?;
    }
    let project = ctx.config.project.render(name);
    match &ctx.config.runtime {
        Runtime::Compose { .. } => {
            let Some(container) = &lease.container else {
                bail!("{name} has no containers; `crumb up` starts it");
            };
            ctx.host_command(&format!("docker restart {}", quote(container)))?;
            ctx.out.step("Restarted", container);
            let service = ctx.config.service.as_deref().unwrap_or_default();
            let took = wait_healthy(ctx, &project, service)?;
            ctx.out
                .step("Healthy", &format!("{service} in {}", duration(took)));
        }
        Runtime::Process { .. } => {
            ctx.host_command(&format!(
                "tmux respawn-pane -k -t {}",
                quote(&format!("={project}:"))
            ))?;
            ctx.out
                .step("Restarted", &format!("tmux session {project}"));
            wait_ready(ctx, name, &lease.vars(ctx.config))?;
        }
    }
    Ok(())
}

/// Stops the backend and pauses the sync and forward. Keeps everything.
pub fn stop(ctx: &Ctx, name: &str) -> Result<()> {
    let (_, lease) = ctx.require(name)?;
    stop_lease(ctx, &lease)
}

pub(super) fn stop_lease(ctx: &Ctx, lease: &Lease) -> Result<()> {
    let project = ctx.config.project.render(&lease.name);
    match &ctx.config.runtime {
        Runtime::Compose { .. } => {
            if lease.container.is_some() {
                ctx.host_command(&format!("docker compose -p {} stop", quote(&project)))?;
                ctx.out.step("Stopped", &format!("containers of {project}"));
            }
        }
        Runtime::Process { .. } => {
            // Signal the command's process group and keep the dead pane, so
            // the lease reads "stopped" and `up` reuses its port.
            ctx.host_command(&format!(
                "pid=$(tmux display-message -p -t {} '#{{pane_pid}}' 2>/dev/null) && [ -n \"$pid\" ] && kill -TERM -- -\"$pid\" 2>/dev/null || true",
                quote(&format!("={project}:"))
            ))?;
            ctx.out.step("Stopped", &format!("tmux session {project}"));
        }
    }
    if let Some(sync) = lease.sync.as_ref().filter(|s| !s.paused) {
        mutagen::run(
            ctx.runner,
            &["sync".into(), "pause".into(), sync.session.clone()],
        )?;
        ctx.out.step("Paused", &format!("sync {}", sync.session));
    }
    if let Some(forward) = lease.forward.as_ref().filter(|f| !f.paused) {
        mutagen::run(
            ctx.runner,
            &["forward".into(), "pause".into(), forward.session.clone()],
        )?;
        ctx.out
            .step("Paused", &format!("forward {}", forward.session));
    }
    Ok(())
}

/// Removes containers, forward, sync, replica and wiring. Keeps the database.
pub fn down(ctx: &Ctx, name: &str) -> Result<()> {
    let (_, lease) = ctx.require(name)?;
    down_lease(ctx, &lease)?;
    if let Some(db) = &lease.database {
        ctx.out.step(
            "Kept",
            &format!("database {} (crumb drop removes it)", db.name),
        );
    }
    Ok(())
}

pub(super) fn down_lease(ctx: &Ctx, lease: &Lease) -> Result<()> {
    let name = lease.name.as_str();
    check_name(name)?;
    let project = ctx.config.project.render(name);
    match &ctx.config.runtime {
        Runtime::Compose { .. } => {
            if lease.container.is_some() {
                ctx.host_command(&format!(
                    "docker compose -p {} down --volumes --remove-orphans",
                    quote(&project)
                ))?;
                ctx.out.step("Removed", &format!("containers of {project}"));
            }
        }
        Runtime::Process { .. } => {
            if lease.state != crate::lease::State::Absent {
                ctx.host_command(&format!(
                    "pid=$(tmux display-message -p -t {p} '#{{pane_pid}}' 2>/dev/null); [ -z \"$pid\" ] || kill -TERM -- -\"$pid\" 2>/dev/null; tmux kill-session -t {s} 2>/dev/null || true",
                    p = quote(&format!("={project}:")),
                    s = quote(&format!("={project}")),
                ))?;
                ctx.out.step("Removed", &format!("tmux session {project}"));
            }
        }
    }
    if let Some(forward) = &lease.forward {
        mutagen::run(
            ctx.runner,
            &[
                "forward".into(),
                "terminate".into(),
                forward.session.clone(),
            ],
        )?;
        ctx.out
            .step("Removed", &format!("forward {}", forward.session));
    }
    let mut beta = None;
    if let Some(sync) = &lease.sync {
        beta = mutagen::sync_session(ctx.runner, &sync.session)?.map(|s| s.beta.path);
        mutagen::run(
            ctx.runner,
            &["sync".into(), "terminate".into(), sync.session.clone()],
        )?;
        ctx.out.step("Removed", &format!("sync {}", sync.session));
    }
    // The replica, the lease's compose state, and the slot file the old
    // script kept. The replica path must end in the lease name.
    let mut paths = vec![format!("{}/{name}", state_root(ctx.config))];
    if let Host::Ssh(_) = ctx.config.host {
        let replica = beta
            .filter(|path| path.ends_with(&format!("/{name}")))
            .unwrap_or_else(|| replica(ctx.config, name));
        paths.push(replica);
        paths.push(format!(
            "{}/.slots/{name}",
            ctx.config.code_root.trim_end_matches('/')
        ));
    }
    let targets: Vec<String> = paths.iter().map(|p| shell_path(p)).collect();
    ctx.host_command(&format!("rm -rf -- {}", targets.join(" ")))?;
    if ctx.config.mutagen {
        ctx.out.step("Removed", &format!("replica {}", paths[1]));
    }
    if let Some(worktree) = lease.live_worktree() {
        unwire(ctx, Path::new(worktree))?;
    }
    Ok(())
}

/// `down`, then drops the database. The caller confirms first.
/// `worktree` is where a drop command runs when the lease has none left.
pub fn drop(ctx: &Ctx, name: &str, worktree: Option<&Path>) -> Result<()> {
    // A database only a command knows about has no row once the rest is down.
    let lease = match ctx.lease(name)?.1 {
        Some(lease) => {
            down_lease(ctx, &lease)?;
            lease
        }
        None if ctx
            .config
            .database
            .as_ref()
            .is_some_and(|db| db.drop.is_some()) =>
        {
            let mut lease = Lease::new(name);
            lease.worktree = worktree.map(|w| crate::lease::Worktree {
                path: w.to_string_lossy().into_owned(),
                exists: true,
            });
            lease
        }
        None => bail!("no lease named {name} on {}", ctx.config.host.label()),
    };
    let Some(db) = &ctx.config.database else {
        return Ok(());
    };
    let database = db.name.render(name);
    match &db.drop {
        Some(command) => {
            let cwd = lease
                .live_worktree()
                .map(Path::new)
                .or(ctx.main)
                .context("database.drop runs in a worktree, and none is left")?;
            run_command(
                ctx,
                "database.drop",
                command,
                &lease.vars(ctx.config),
                cwd,
                "database",
            )?;
        }
        None => {
            psql(
                ctx,
                "postgres",
                &format!(
                    "drop database if exists {} with (force);",
                    sql_ident(&database)
                ),
            )?;
        }
    }
    ctx.out.step("Dropped", &format!("database {database}"));
    Ok(())
}

/// Applies the worktree's migrations to the lease database, then restarts.
pub fn migrate(ctx: &Ctx, name: &str) -> Result<()> {
    let (_, lease) = ctx.require(name)?;
    let command = ctx
        .config
        .database
        .as_ref()
        .and_then(|db| db.migrate.as_deref())
        .context("database.migrate is not set")?;
    let worktree = lease
        .live_worktree()
        .context("migrations run from the lease's worktree, which is gone")?;
    ctx.out.step("Migrating", &format!("database for {name}"));
    run_command(
        ctx,
        "database.migrate",
        command,
        &lease.vars(ctx.config),
        Path::new(worktree),
        "database",
    )?;
    ctx.out.step("Migrated", name);
    if lease.is_running() {
        restart(ctx, name)?;
    }
    Ok(())
}

/// Recreates the forward and rewrites the wired files.
pub fn tunnel(ctx: &Ctx, name: &str) -> Result<()> {
    let (_, mut lease) = ctx.require(name)?;
    let port = lease
        .port
        .with_context(|| format!("{name} has no port; `crumb up` starts it"))?;
    if ctx.config.forward == Forward::Mutagen {
        lease.local_port = Some(ensure_forward(ctx, name, port, lease.forward.as_ref())?);
    }
    let worktree = lease
        .live_worktree()
        .context("the lease's worktree is gone")?
        .to_string();
    wire_all(ctx, name, Path::new(&worktree), &lease.vars(ctx.config))
}
