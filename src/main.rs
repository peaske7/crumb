mod checks;
mod config;
mod lease;
mod ls;
mod mutagen;
mod probe;
mod run;
mod snapshot;
mod tui;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// A fast, minimal TUI and CLI for per-worktree development backends.
#[derive(Parser)]
#[command(name = "crumb", version)]
struct Cli {
    /// Read this file instead of the nearest crumb.toml.
    #[arg(long, global = true, value_name = "PATH")]
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
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("crumb: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let config = config::load(cli.config.as_deref(), cli.host.as_deref())?;
    match cli.command {
        None => tui::run(config),
        Some(Command::Ls { json }) => {
            let runner = run::Runner::default();
            let result = snapshot::collect(&config, &runner);
            if cli.verbose {
                for record in runner.records() {
                    eprintln!("{}", record.line());
                }
            }
            ls::print(&result?, json)
        }
    }
}
