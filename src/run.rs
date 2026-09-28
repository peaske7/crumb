use std::collections::VecDeque;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jiff::Timestamp;

use crate::config::Host;

const LOG_LIMIT: usize = 200;

/// One external command crumb ran, as shown in the command log.
#[derive(Debug, Clone)]
pub struct Record {
    pub at: Timestamp,
    pub display: String,
    pub duration: Duration,
    pub exit: Option<i32>,
}

impl Record {
    /// `21:44:03    58 ms  ssh indigo bash -s < probe.sh`, or `exit 1` on failure.
    pub fn line(&self) -> String {
        let time = self
            .at
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M:%S");
        let outcome = match self.exit {
            Some(0) => format!("{:>5} ms", self.duration.as_millis()),
            Some(code) => format!("exit {code:<3}"),
            None => "no start".to_string(),
        };
        format!("{time}  {outcome:>8}  {}", self.display)
    }
}

/// Runs every external command crumb uses and records it. Cloning shares the log.
#[derive(Clone, Default)]
pub struct Runner {
    log: Arc<Mutex<VecDeque<Record>>>,
}

impl Runner {
    pub fn records(&self) -> Vec<Record> {
        self.log.lock().expect("log lock").iter().cloned().collect()
    }

    /// Runs `program args…`, feeding `stdin` when given. `display` is what the
    /// command log shows; it must be a command a person could paste.
    pub fn run(
        &self,
        display: String,
        program: &str,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<Output> {
        let started = Instant::now();
        let at = Timestamp::now();
        let result = spawn(program, args, stdin);
        let exit = match &result {
            Ok(output) => output.status.code(),
            Err(_) => None,
        };
        self.push(Record {
            at,
            display,
            duration: started.elapsed(),
            exit,
        });
        result.with_context(|| format!("running {program}"))
    }

    /// Runs a bash script on the host: locally, or over one shared SSH connection.
    pub fn script(&self, host: &Host, name: &str, script: &str) -> Result<Output> {
        match host {
            Host::Local => self.run(
                format!("bash -s < {name}"),
                "bash",
                &["-s".to_string()],
                Some(script.as_bytes()),
            ),
            Host::Ssh(alias) => {
                let mut args = ssh_options();
                args.push(alias.clone());
                args.push("bash -s".to_string());
                self.run(
                    format!("ssh {alias} bash -s < {name}"),
                    "ssh",
                    &args,
                    Some(script.as_bytes()),
                )
            }
        }
    }

    fn push(&self, record: Record) {
        let mut log = self.log.lock().expect("log lock");
        if log.len() == LOG_LIMIT {
            log.pop_front();
        }
        log.push_back(record);
    }
}

/// Options for every SSH call: batch mode, no forwards inherited from the host
/// alias (they belong to the user's own tunnels), and one master connection
/// kept open so later calls skip the handshake.
fn ssh_options() -> Vec<String> {
    let control = std::env::var_os("HOME")
        .map(|home| format!("{}/.ssh/crumb-%C", home.to_string_lossy()))
        .unwrap_or_else(|| "/tmp/crumb-%C".to_string());
    [
        "BatchMode=yes",
        "ClearAllForwardings=yes",
        "ConnectTimeout=10",
        "ServerAliveInterval=10",
        "ControlMaster=auto",
        "ControlPersist=30m",
    ]
    .into_iter()
    .map(str::to_string)
    .chain(std::iter::once(format!("ControlPath={control}")))
    .flat_map(|option| ["-o".to_string(), option])
    .collect()
}

fn spawn(program: &str, args: &[String], stdin: Option<&[u8]>) -> std::io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(bytes) = stdin {
        // Dropping the handle closes stdin so the child sees EOF.
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(bytes)?;
    }
    child.wait_with_output()
}
