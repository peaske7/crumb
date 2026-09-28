use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
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
        self.run_with(
            display,
            program,
            args,
            Options {
                stdin,
                ..Options::default()
            },
        )
    }

    /// [`Runner::run`] with an environment, a working directory, and each
    /// stderr line handed to `on_stderr` as it arrives.
    pub fn run_with(
        &self,
        display: String,
        program: &str,
        args: &[String],
        options: Options,
    ) -> Result<Output> {
        let started = Instant::now();
        let at = Timestamp::now();
        let result = spawn(program, args, &options);
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

    /// Runs a bash script on the host, handing each stderr line to `on_stderr`
    /// as it arrives, for long steps whose progress is worth showing.
    pub fn script_streaming(
        &self,
        host: &Host,
        name: &str,
        script: &str,
        on_stderr: &(dyn Fn(&str) + Sync),
    ) -> Result<Output> {
        let (display, program, args) = on_host(host, "bash -s");
        self.run_with(
            format!("{display} < {name}"),
            program,
            &args,
            Options {
                stdin: Some(script.as_bytes()),
                on_stderr: Some(on_stderr),
                ..Options::default()
            },
        )
    }

    /// Runs one shell command on the host.
    pub fn command(&self, host: &Host, command: &str) -> Result<Output> {
        let (display, program, args) = on_host(host, command);
        self.run(display, program, &args, None)
    }

    /// Runs a command on the host with this terminal as its stdin and output,
    /// for `crumb logs -f`.
    pub fn attach(&self, host: &Host, command: &str) -> Result<ExitStatus> {
        let (display, program, args) = on_host(host, command);
        let started = Instant::now();
        let at = Timestamp::now();
        let status = Command::new(program).args(&args).status();
        self.push(Record {
            at,
            display,
            duration: started.elapsed(),
            outcome: match &status {
                Ok(status) => status.code().map_or(Outcome::Stopped, Outcome::Exited),
                Err(_) => Outcome::Failed,
            },
        });
        status.with_context(|| format!("running {program}"))
    }

    /// Runs a shell command on this machine, in `cwd`, with extra environment.
    /// Each output line goes to `on_line` as it arrives.
    pub fn local(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        on_stderr: &(dyn Fn(&str) + Sync),
    ) -> Result<Output> {
        self.run_with(
            command.to_string(),
            "sh",
            &["-c".to_string(), command.to_string()],
            Options {
                cwd: Some(cwd),
                env,
                on_stderr: Some(on_stderr),
                on_stdout: Some(on_stderr),
                ..Options::default()
            },
        )
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

/// How to run a command beyond its arguments.
#[derive(Default)]
pub struct Options<'a> {
    pub stdin: Option<&'a [u8]>,
    pub cwd: Option<&'a Path>,
    pub env: &'a [(String, String)],
    pub on_stderr: Option<&'a (dyn Fn(&str) + Sync)>,
    pub on_stdout: Option<&'a (dyn Fn(&str) + Sync)>,
}

/// Single-quotes a value for a POSIX shell.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
pub fn ssh_options() -> Vec<String> {
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

fn spawn(program: &str, args: &[String], options: &Options) -> std::io::Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(options.env.iter().map(|(k, v)| (k, v)))
        .stdin(if options.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = options.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // Feed stdin and drain both pipes at once, so a chatty command never
    // blocks on a full pipe.
    let (out, err) = thread::scope(|scope| {
        if let (Some(mut pipe), Some(bytes)) = (stdin, options.stdin) {
            scope.spawn(move || {
                // Dropping the handle closes stdin so the child sees EOF.
                let _ = pipe.write_all(bytes);
            });
        }
        let err = scope.spawn(|| match options.on_stderr {
            Some(on_stderr) => read_lines(stderr, on_stderr),
            None => {
                let mut collected = Vec::new();
                let _ = BufReader::new(stderr).read_to_end(&mut collected);
                collected
            }
        });
        let out = match options.on_stdout {
            Some(on_stdout) => read_lines(stdout, on_stdout),
            None => {
                let mut out = Vec::new();
                let _ = stdout.read_to_end(&mut out);
                out
            }
        };
        (out, err.join().unwrap_or_default())
    });
    let status: ExitStatus = child.wait()?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Reads a pipe to the end, handing each line to `on_line` as it arrives.
fn read_lines(pipe: impl Read, on_line: &(dyn Fn(&str) + Sync)) -> Vec<u8> {
    let mut collected = Vec::new();
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
        on_line(String::from_utf8_lossy(&line).trim_end());
        collected.extend_from_slice(&line);
        line.clear();
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_values_for_the_shell() {
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

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
    fn local_commands_get_env_cwd_and_stream_stderr() {
        let runner = Runner::default();
        let seen = Mutex::new(Vec::new());
        let dir = std::env::temp_dir();
        let output = runner
            .local(
                "echo \"$CRUMB_LEASE\"; pwd; echo progress >&2",
                &dir,
                &[("CRUMB_LEASE".into(), "a".into())],
                &|line| seen.lock().unwrap().push(line.to_string()),
            )
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("a\n"));
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"progress".to_string()));
        assert!(seen.contains(&"a".to_string()));
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
