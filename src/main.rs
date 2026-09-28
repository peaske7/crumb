mod agents;
mod checks;
mod cli;
mod config;
mod doctor;
mod init;
mod lease;
mod ls;
mod mutagen;
mod ops;
mod probe;
mod run;
mod snapshot;
mod template;
mod tui;
mod view;
mod wire;
mod worktree;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::ops::Ctx;
use crate::run::Runner;
use crate::snapshot::Here;
use crate::worktree::Checkout;

/// A fast, minimal TUI and CLI for per-worktree development backends.
#[derive(Parser)]
#[command(name = "crumb", version)]
struct Cli {
    /// Read this file instead of the nearest crumb.toml.
    #[arg(long, global = true, value_name = "PATH", env = "CRUMB_CONFIG")]
    config: Option<PathBuf>,

    /// Where leases run: "local" or "ssh://<host>". Overrides the config.
    #[arg(long, global = true, value_name = "HOST")]
    host: Option<String>,

    /// Print every command crumb ran, with its duration, to stderr.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List leases as text, or as JSON for scripts and agents.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Start this worktree's backend: database, sync, port, runtime, forward,
    /// wiring. Safe to rerun; each step continues where the last run stopped.
    Up {
        /// The lease name. Defaults to the worktree directory's name.
        #[arg(long)]
        name: Option<String>,
        /// The worktree. Defaults to the one you are in.
        path: Option<PathBuf>,
    },
    /// Wait for the sync, restart the backend, wait until it is healthy.
    Restart { name: Option<String> },
    /// Stop the backend and pause its sync and forward. Keeps everything.
    Stop { name: Option<String> },
    /// Remove containers, forward, sync, replica and wiring. Keeps the database.
    Down {
        name: Option<String>,
        /// Succeed quietly when there is no such lease (for removal hooks).
        #[arg(long)]
        if_exists: bool,
    },
    /// `down`, then drop the lease's database.
    Drop {
        name: Option<String>,
        /// Skip the question by naming the lease again.
        #[arg(long, value_name = "NAME")]
        confirm: Option<String>,
    },
    /// Apply the worktree's migrations to the lease database, then restart.
    Migrate { name: Option<String> },
    /// Recreate the port forward and rewrite the wired env files.
    Tunnel { name: Option<String> },
    /// Stop orphaned leases; bring down those stopped for seven days.
    Reap {
        /// Apply without asking.
        #[arg(long)]
        yes: bool,
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a crumb.toml for this repository from what it contains.
    Init {
        /// Take the detected answers without asking.
        #[arg(long)]
        yes: bool,
        /// Replace an existing crumb.toml.
        #[arg(long)]
        force: bool,
    },
    /// Check each configured piece and print the fix for each failure.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Install the crumb skill for coding agents (Claude Code, Codex).
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Print the backend's log.
    Logs {
        name: Option<String>,
        /// Keep printing new lines.
        #[arg(long, short)]
        follow: bool,
        #[arg(long, default_value_t = 200)]
        tail: usize,
    },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Write the skill for each agent set up on this machine.
    Install {
        #[arg(long, default_value = "all")]
        client: String,
        /// Into this repository instead of your home directory.
        #[arg(long)]
        project: bool,
    },
    /// Remove the skill.
    Uninstall {
        #[arg(long, default_value = "all")]
        client: String,
        #[arg(long)]
        project: bool,
    },
    /// Say whether the skill is installed and current.
    Status {
        #[arg(long, default_value = "all")]
        client: String,
        #[arg(long)]
        project: bool,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let verbose = cli.verbose;
    let runner = Runner::default();
    let result = run(cli, &runner);
    if verbose {
        for record in runner.records() {
            eprintln!("{}", record.line());
        }
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("crumb: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, runner: &Runner) -> Result<()> {
    // These work without a crumb.toml.
    match &cli.command {
        Some(Command::Init { yes, force }) => return init(runner, *yes, *force),
        Some(Command::Agents { action }) => return agents(runner, action),
        _ => {}
    }
    let checkout = worktree::find(runner, &std::env::current_dir()?);
    let config = config::load(
        cli.config.as_deref(),
        cli.host.as_deref(),
        checkout.as_ref().map(|c| c.main.as_path()),
    )?;
    if config.repo.is_none() && !matches!(cli.command, Some(Command::Doctor { .. })) {
        bail!(
            "no crumb.toml in this repository or its main checkout; `crumb init` writes one, or pass --config"
        );
    }
    let here = checkout.as_ref().filter(|c| !c.is_main()).map(|c| Here {
        lease: c.lease(),
        worktree: c.root.clone(),
    });
    let Some(command) = cli.command else {
        return tui::run(config, runner.clone(), checkout, here);
    };
    let printer = cli::Printer::new();
    let ctx = Ctx {
        config: &config,
        runner,
        out: &printer,
        main: checkout.as_ref().map(|c| c.main.as_path()),
    };
    match command {
        Command::Ls { json } => {
            let snapshot = snapshot::collect(&config, runner, here.as_ref())?;
            ls::print(&snapshot, json)
        }
        Command::Up { name, path } => {
            let checkout = match path {
                Some(path) => worktree::find(runner, &path)
                    .with_context(|| format!("{} is not in a git worktree", path.display()))?,
                None => checkout
                    .clone()
                    .context("run crumb up inside a git worktree, or pass its path")?,
            };
            let lease = match name {
                Some(name) => name,
                None if checkout.is_main() => {
                    bail!("this is the main checkout; pass --name to give it a lease anyway")
                }
                None => lease_for(&ctx, &checkout)?,
            };
            ops::up(&ctx, &lease, &checkout.root)
        }
        Command::Restart { name } => ops::restart(&ctx, &name_or_here(&ctx, name, &checkout)?),
        Command::Stop { name } => ops::stop(&ctx, &name_or_here(&ctx, name, &checkout)?),
        Command::Down { name, if_exists } => {
            let name = name_or_here(&ctx, name, &checkout)?;
            if if_exists && ctx.lease(&name)?.1.is_none() {
                return Ok(());
            }
            ops::down(&ctx, &name)
        }
        Command::Drop { name, confirm } => {
            let name = name_or_here(&ctx, name, &checkout)?;
            let database = config
                .database
                .as_ref()
                .map(|db| db.name.render(&name))
                .unwrap_or_default();
            let confirmed = match confirm {
                Some(typed) => typed == name,
                None => cli::confirm_typed(
                    &format!("This drops database {database}. Type {name} to confirm"),
                    &name,
                )?,
            };
            if !confirmed {
                bail!("not confirmed; nothing changed");
            }
            ops::drop(&ctx, &name, checkout.as_ref().map(|c| c.root.as_path()))
        }
        Command::Migrate { name } => ops::migrate(&ctx, &name_or_here(&ctx, name, &checkout)?),
        Command::Tunnel { name } => ops::tunnel(&ctx, &name_or_here(&ctx, name, &checkout)?),
        Command::Reap { yes, dry_run } => reap(&ctx, yes, dry_run),
        Command::Logs { name, follow, tail } => {
            let name = name_or_here(&ctx, name, &checkout)?;
            logs(&ctx, &name, follow, tail)
        }
        Command::Doctor { json } => {
            let checks = doctor::run(&config, runner, ctx.main);
            if json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                let color = std::io::IsTerminal::is_terminal(&std::io::stdout())
                    && std::env::var_os("NO_COLOR").is_none();
                doctor::print(&checks, color);
            }
            let failed = checks.iter().filter(|c| !c.ok).count();
            if failed > 0 {
                bail!(
                    "{failed} check{} failed",
                    if failed == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
        Command::Init { .. } | Command::Agents { .. } => unreachable!("handled above"),
    }
}

fn init(runner: &Runner, yes: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = worktree::find(runner, &cwd).map_or(cwd, |c| c.root);
    let path = repo.join(config::PROJECT_FILE);
    if path.exists() && !force {
        bail!("{} exists; pass --force to replace it", path.display());
    }
    let detected = init::detect(&repo);
    let answers = init::ask(&detected, &repo, yes)?;
    std::fs::write(&path, init::render(&detected, &answers))?;
    eprintln!("wrote {}", path.display());
    let config = config::load(Some(&path), None, None)?;
    let runner = Runner::default();
    let checkout = worktree::find(&runner, &repo);
    let checks = doctor::run(
        &config,
        &runner,
        checkout.as_ref().map(|c| c.main.as_path()),
    );
    doctor::print(
        &checks,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
    );
    let base = agents::base(&runner, false)?;
    let missing = agents::status(&base, &agents::Client::ALL, false)
        .iter()
        .any(|s| matches!(s.state, agents::State::Missing | agents::State::Outdated));
    if missing {
        eprintln!("tip: `crumb agents install` teaches Claude Code and Codex to use crumb");
    }
    Ok(())
}

fn agents(runner: &Runner, action: &AgentsAction) -> Result<()> {
    match action {
        AgentsAction::Install { client, project } => {
            let base = agents::base(runner, *project)?;
            for status in agents::install(&base, &agents::Client::parse(client)?, *project)? {
                match status.state {
                    agents::State::NoClient => eprintln!(
                        "skipped {}: not set up on this machine",
                        status.client.label()
                    ),
                    _ => eprintln!("installed {}", status.path.display()),
                }
            }
            Ok(())
        }
        AgentsAction::Uninstall { client, project } => {
            let base = agents::base(runner, *project)?;
            let removed = agents::uninstall(&base, &agents::Client::parse(client)?)?;
            if removed.is_empty() {
                eprintln!("nothing to remove");
            }
            for path in removed {
                eprintln!("removed {}", path.display());
            }
            Ok(())
        }
        AgentsAction::Status {
            client,
            project,
            json,
        } => {
            let base = agents::base(runner, *project)?;
            let states = agents::status(&base, &agents::Client::parse(client)?, *project);
            if *json {
                println!("{}", serde_json::to_string_pretty(&states)?);
                return Ok(());
            }
            for status in &states {
                let state = match status.state {
                    agents::State::Current => "current",
                    agents::State::Outdated => "outdated: `crumb agents install` refreshes it",
                    agents::State::Missing => "not installed",
                    agents::State::NoClient => "no client on this machine",
                };
                println!(
                    "{:<12} {state}  {}",
                    status.client.label(),
                    status.path.display()
                );
            }
            Ok(())
        }
    }
}

/// The lease for a worktree: the one already running from it (a lease an
/// older tool named differently), else the directory's name.
fn lease_for(ctx: &Ctx, checkout: &Checkout) -> Result<String> {
    let snapshot = snapshot::collect(ctx.config, ctx.runner, None)?;
    let root = checkout.root.to_string_lossy();
    Ok(snapshot
        .leases
        .iter()
        .find(|l| l.live_worktree() == Some(root.as_ref()))
        .map(|l| l.name.clone())
        .unwrap_or_else(|| checkout.lease()))
}

fn name_or_here(ctx: &Ctx, name: Option<String>, checkout: &Option<Checkout>) -> Result<String> {
    match (name, checkout) {
        (Some(name), _) => Ok(name),
        (None, Some(checkout)) => lease_for(ctx, checkout),
        (None, None) => bail!("name a lease, or run this inside its worktree"),
    }
}

fn reap(ctx: &Ctx, yes: bool, dry_run: bool) -> Result<()> {
    let snapshot = snapshot::collect(ctx.config, ctx.runner, None)?;
    let plan = ops::reap::plan(&snapshot, jiff::Timestamp::now());
    if plan.items.is_empty() {
        eprintln!("no orphaned leases on {}", snapshot.host);
        return Ok(());
    }
    let rows = plan.rows();
    let width = rows.iter().map(|(_, l, _)| l.len()).max().unwrap_or(0);
    println!("reap plan · {}", snapshot.host);
    for (verb, lease, detail) in &rows {
        println!("  {verb:<5} {lease:<width$}   {detail}");
    }
    if dry_run || plan.changes() == 0 {
        return Ok(());
    }
    if !yes && !cli::confirm("apply?")? {
        bail!("not applied; nothing changed");
    }
    let failures = ops::reap::apply(ctx, &plan);
    if !failures.is_empty() {
        bail!("{} of {} leases failed", failures.len(), plan.changes());
    }
    Ok(())
}

fn logs(ctx: &Ctx, name: &str, follow: bool, tail: usize) -> Result<()> {
    let (_, lease) = ctx.lease(name)?;
    let lease = lease.with_context(|| format!("no lease named {name}"))?;
    let command = ops::log_command(ctx.config, &lease, follow, tail)
        .with_context(|| format!("{name} has no backend to read a log from"))?;
    let status = ctx.runner.attach(&ctx.config.host, &command)?;
    if !status.success() {
        bail!("docker logs exited with {status}");
    }
    Ok(())
}
