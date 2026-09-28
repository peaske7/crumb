use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;

use crate::config::Config;
use crate::lease::{self, Lease};
use crate::mutagen;
use crate::probe::{self, Mem};
use crate::run::Runner;

/// Everything one refresh learned.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: Timestamp,
    pub host: String,
    pub mem: Option<Mem>,
    pub leases: Vec<Lease>,
    pub warnings: Vec<String>,
}

/// Reads the host and Mutagen in parallel and joins them.
pub fn collect(config: &Config, runner: &Runner) -> Result<Snapshot> {
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
    let leases = lease::join(config, &host, &syncs, |path| Path::new(path).exists());
    Ok(Snapshot {
        at: Timestamp::now(),
        host: config.host.label().to_string(),
        mem: host.mem,
        leases,
        warnings,
    })
}
