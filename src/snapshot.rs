use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;

use crate::checks::{self, Images};
use crate::config::Config;
use crate::lease::{self, Lease};
use crate::mutagen::{self, SyncSession};
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
    at: Timestamp,
    host: HostFacts,
    syncs: Vec<SyncSession>,
    warnings: Vec<String>,
}

/// Reads the host and Mutagen in parallel.
pub fn gather(config: &Config, runner: &Runner) -> Result<Facts> {
    let (host, syncs) = std::thread::scope(|scope| {
        let syncs = config
            .mutagen
            .then(|| scope.spawn(|| mutagen::list(runner)));
        let host = probe::run(config, runner);
        let syncs = syncs.map(|handle| handle.join().expect("mutagen thread panicked"));
        (host, syncs)
    });
    let host = host?;
    let mut warnings = host.warnings.clone();
    let syncs = match syncs {
        None => Vec::new(),
        Some(Ok(sessions)) => sessions,
        Some(Err(err)) => {
            warnings.push(format!("{err:#}"));
            Vec::new()
        }
    };
    Ok(Facts {
        at: Timestamp::now(),
        host,
        syncs,
        warnings,
    })
}

/// Joins the facts into leases and runs the local checks. Cheap enough to run
/// again once more image facts arrive.
pub fn assemble(config: &Config, facts: &Facts, images: &Images) -> Snapshot {
    let mut leases = lease::join(config, &facts.host, &facts.syncs, |path| {
        Path::new(path).exists()
    });
    checks::apply(config, images, &mut leases);
    Snapshot {
        at: facts.at,
        host: config.host.label().to_string(),
        mem: facts.host.mem,
        leases,
        warnings: facts.warnings.clone(),
    }
}

/// A complete snapshot in one call, for `crumb ls`: waits for image facts.
pub fn collect(config: &Config, runner: &Runner) -> Result<Snapshot> {
    let facts = gather(config, runner)?;
    let mut images = Images::default();
    let snapshot = assemble(config, &facts, &images);
    let missing = images.missing(config, &snapshot.leases);
    if missing.is_empty() {
        return Ok(snapshot);
    }
    for id in &missing {
        images.fetch(runner, config, id);
    }
    Ok(assemble(config, &facts, &images))
}
