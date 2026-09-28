use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
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
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    /// crumb stopped it, as when you leave the logs view.
    Stopped,
    /// It never started, or was killed by a signal crumb did not send.
    Failed,
}

impl Record {
    pub fn ok(&self) -> bool {
        matches!(self.outcome, Outcome::Exited(0) | Outcome::Stopped)
    }

    /// `58 ms`, `exit 1`, `stopped`, or `failed`.
    pub fn result(&self) -> String {
        match self.outcome {
            Outcome::Exited(0) => duration(self.duration),
            Outcome::Exited(code) => format!("exit {code}"),
            Outcome::Stopped => "stopped".to_string(),
            Outcome::Failed => "failed".to_string(),
        }
    }

    /// `21:44:03     58 ms  ssh indigo bash -s < probe.sh`
    pub fn line(&self) -> String {
        let time = self
            .at
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M:%S");
        format!("{time}  {:>8}  {}", self.result(), self.display)
    }
}

/// `58 ms`, `4.1 s`, `2m 3s`.
pub fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    match ms {
        ms if ms < 1_000 => format!("{ms} ms"),
        ms if ms < 60_000 => format!("{:.1} s", ms as f64 / 1_000.0),
        ms => format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000),
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
        let outcome = match &result {
            Ok(output) => output
                .status
                .code()
                .map_or(Outcome::Failed, Outcome::Exited),
            Err(_) => Outcome::Failed,
        };
        self.push(Record {
            at,
            display,
            duration: started.elapsed(),
            outcome,
        });
        result.with_context(|| format!("running {program}"))
    }

    /// Runs a bash script on the host: locally, or over one shared SSH connection.
    pub fn script(&self, host: &Host, name: &str, script: &str) -> Result<Output> {
        let (display, program, args) = on_host(host, "bash -s");
        self.run(
            format!("{display} < {name}"),
            program,
            &args,
            Some(script.as_bytes()),
        )
    }

    /// Runs one shell command on the host.
    pub fn command(&self, host: &Host, command: &str) -> Result<Output> {
        let (display, program, args) = on_host(host, command);
        self.run(display, program, &args, None)
    }

    /// Starts a long-running command on the host and hands each output line to
    /// `on_line` from a background thread. Dropping the stream stops it.
    pub fn stream(
        &self,
        host: &Host,
        command: &str,
        on_line: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Stream> {
        let (display, program, args) = on_host(host, command);
        let mut child = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {program}"))?;
        let on_line = Arc::new(on_line);
        forward_lines(child.stdout.take(), Arc::clone(&on_line));
        forward_lines(child.stderr.take(), on_line);
        Ok(Stream {
            child,
            at: Timestamp::now(),
            started: Instant::now(),
            display,
            runner: self.clone(),
        })
    }

    fn push(&self, record: Record) {
        let mut log = self.log.lock().expect("log lock");
        if log.len() == LOG_LIMIT {
            log.pop_front();
        }
        log.push_back(record);
    }
}

/// A running command started by [`Runner::stream`].
pub struct Stream {
    child: Child,
    at: Timestamp,
    started: Instant,
    display: String,
    runner: Runner,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // If it already exited on its own, keep its real exit code.
        let outcome = match self.child.try_wait() {
            Ok(Some(status)) => status.code().map_or(Outcome::Failed, Outcome::Exited),
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                Outcome::Stopped
            }
        };
        self.runner.push(Record {
            at: self.at,
            display: std::mem::take(&mut self.display),
            duration: self.started.elapsed(),
            outcome,
        });
    }
}

fn forward_lines(
    pipe: Option<impl Read + Send + 'static>,
    on_line: Arc<impl Fn(String) + Send + Sync + 'static>,
) {
    if let Some(pipe) = pipe {
        thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                on_line(line);
            }
        });
    }
}

/// How to run `command` on the host, and how the command log shows it.
fn on_host(host: &Host, command: &str) -> (String, &'static str, Vec<String>) {
    match host {
        Host::Local => (
            command.to_string(),
            "sh",
            vec!["-c".to_string(), command.to_string()],
        ),
        Host::Ssh(alias) => {
            let mut args = ssh_options();
            args.push(alias.clone());
            args.push(command.to_string());
            (format!("ssh {alias} {command}"), "ssh", args)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(duration(Duration::from_millis(58)), "58 ms");
        assert_eq!(duration(Duration::from_millis(4_120)), "4.1 s");
        assert_eq!(duration(Duration::from_secs(123)), "2m 3s");
    }

    #[test]
    fn a_stream_delivers_lines_and_is_recorded_when_dropped() {
        let runner = Runner::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let stream = runner
            .stream(
                &Host::Local,
                "printf 'one\\ntwo\\n'; echo err >&2",
                move |line| {
                    let _ = tx.send(line);
                },
            )
            .unwrap();
        let mut lines: Vec<String> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        lines.sort();
        assert_eq!(lines, ["err", "one", "two"]);
        drop(stream);
        let records = runner.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].display.starts_with("printf"));
        assert!(records[0].ok());
    }

    #[test]
    fn a_failing_command_is_recorded_with_its_exit_code() {
        let runner = Runner::default();
        let output = runner.command(&Host::Local, "exit 3").unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(runner.records()[0].outcome, Outcome::Exited(3));
        assert_eq!(runner.records()[0].result(), "exit 3");
    }
}
