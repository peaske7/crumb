use std::path::{Path, PathBuf};

use anyhow::Result;
use jiff::Timestamp;

use crate::checks::{self, Images};
use crate::config::{Config, Forward};
use crate::lease::{self, Lease, Worktree};
use crate::mutagen::{self, ForwardSession, SyncSession};
use crate::probe::{self, HostFacts, Mem};
use crate::run::Runner;

/// Everything one refresh learned, joined into lease rows.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: Timestamp,
    pub host: String,
    pub mem: Option<Mem>,
    pub leases: Vec<Lease>,
    pub warnings: Vec<String>,
}

/// What the host and Mutagen reported, before joining.
pub struct Facts {
    pub at: Timestamp,
    pub host: HostFacts,
    pub syncs: Vec<SyncSession>,
    pub forwards: Vec<ForwardSession>,
    pub warnings: Vec<String>,
}

/// The worktree crumb runs in, so it is listed before it has a lease.
#[derive(Debug, Clone)]
pub struct Here {
    pub lease: String,
    pub worktree: PathBuf,
}

/// Reads the host and Mutagen in parallel.
pub fn gather(config: &Config, runner: &Runner) -> Result<Facts> {
    let (host, syncs, forwards) = std::thread::scope(|scope| {
        let syncs = config
            .mutagen
            .then(|| scope.spawn(|| mutagen::list(runner)));
        let forwards = (config.forward == Forward::Mutagen)
            .then(|| scope.spawn(|| mutagen::list_forwards(runner)));
        let host = probe::run(config, runner);
        let syncs = syncs.map(|handle| handle.join().expect("mutagen thread panicked"));
        let forwards = forwards.map(|handle| handle.join().expect("mutagen thread panicked"));
        (host, syncs, forwards)
    });
    let host = host?;
    let mut warnings = host.warnings.clone();
    let syncs = sessions(syncs, &mut warnings);
    let forwards = sessions(forwards, &mut warnings);
    Ok(Facts {
        at: Timestamp::now(),
        host,
        syncs,
        forwards,
        warnings,
    })
}

/// A Mutagen list, or nothing and a warning: the host's facts still stand.
fn sessions<T>(result: Option<Result<Vec<T>>>, warnings: &mut Vec<String>) -> Vec<T> {
    match result {
        None => Vec::new(),
        Some(Ok(sessions)) => sessions,
        Some(Err(err)) => {
            warnings.push(format!("{err:#}"));
            Vec::new()
        }
    }
}

/// Joins the facts into leases and runs the local checks. Cheap enough to run
/// again once more image facts arrive.
pub fn assemble(config: &Config, facts: &Facts, images: &Images, here: Option<&Here>) -> Snapshot {
    let mut leases = lease::join(config, &facts.host, &facts.syncs, &facts.forwards, |path| {
        Path::new(path).exists()
    });
    if let Some(here) = here {
        mark_here(&mut leases, here);
    }
    checks::apply(config, images, &mut leases);
    leases.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
    Snapshot {
        at: facts.at,
        host: config.host.label().to_string(),
        mem: facts.host.mem,
        leases,
        warnings: facts.warnings.clone(),
    }
}

/// Lists this worktree's lease even when nothing runs for it yet.
fn mark_here(leases: &mut Vec<Lease>, here: &Here) {
    let path = here.worktree.to_string_lossy().into_owned();
    let index = match leases.iter().position(|l| l.name == here.lease) {
        Some(index) => index,
        None => {
            leases.push(Lease::new(&here.lease));
            leases.len() - 1
        }
    };
    let lease = &mut leases[index];
    lease.here = true;
    if lease.worktree.is_none() {
        lease.worktree = Some(Worktree { path, exists: true });
    }
    lease.regroup();
}

/// A complete snapshot in one call, for the CLI: waits for image facts.
pub fn collect(config: &Config, runner: &Runner, here: Option<&Here>) -> Result<Snapshot> {
    let facts = gather(config, runner)?;
    let mut images = Images::default();
    let snapshot = assemble(config, &facts, &images, here);
    let missing = images.missing(config, &snapshot.leases);
    if missing.is_empty() {
        return Ok(snapshot);
    }
    for id in &missing {
        images.fetch(runner, config, id);
    }
    Ok(assemble(config, &facts, &images, here))
}
