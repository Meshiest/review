//! Runs the `git` CLI as a child process with a timeout, cancellation and
//! line-by-line stderr progress. Callers run it off the UI thread.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

const POLL: Duration = Duration::from_millis(20);
/// How long to wait for the pipe readers after git exits; a lingering
/// grandchild (ssh multiplexing) can keep a pipe open.
const DRAIN: Duration = Duration::from_secs(2);
const STDERR_TAIL: usize = 4096;
/// Most stdout kept when it is captured: the plumbing commands that need it
/// answer an id or a ref line.
const STDOUT_LIMIT: usize = 64 << 10;

pub(crate) type LineSink = Box<dyn FnMut(&str) + Send>;

/// Where git runs: here, or on another machine through the system `ssh`.
#[derive(Clone, Debug)]
enum Place {
    Local(PathBuf),
    /// `ssh <host> -- git -C <dir> ...`, every word quoted for a POSIX
    /// shell. `dir` is relative to the remote home unless absolute.
    Ssh {
        host: String,
        dir: String,
    },
}

pub(crate) struct GitCommand {
    place: Place,
    args: Vec<OsString>,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    on_stderr_line: Option<LineSink>,
    capture_stdout: bool,
}

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub status: ExitStatus,
    pub stderr: String,
    /// Empty unless [`GitCommand::capture_stdout`] was asked for.
    pub stdout: String,
}

impl GitCommand {
    pub fn new(dir: &Path, timeout: Duration) -> Self {
        Self::at(Place::Local(dir.to_owned()), timeout)
    }

    /// Git on `host` in `dir`, through the system `ssh` (the user's config,
    /// agent and keys). `host` was checked by [`crate::SshHost`].
    pub fn over_ssh(host: &str, dir: &str, timeout: Duration) -> Self {
        Self::at(
            Place::Ssh {
                host: host.to_owned(),
                dir: dir.to_owned(),
            },
            timeout,
        )
    }

    fn at(place: Place, timeout: Duration) -> Self {
        Self {
            place,
            args: Vec::new(),
            timeout,
            cancel: crate::cancel::current(),
            on_stderr_line: None,
            capture_stdout: false,
        }
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn cancel_on(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    #[must_use]
    pub fn on_stderr_line(mut self, sink: LineSink) -> Self {
        self.on_stderr_line = Some(sink);
        self
    }

    /// Keeps what git prints on stdout (up to [`STDOUT_LIMIT`]).
    #[must_use]
    pub fn capture_stdout(mut self) -> Self {
        self.capture_stdout = true;
        self
    }

    fn display_args(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        match &self.place {
            Place::Local(_) => args,
            Place::Ssh { host, .. } => format!("{args} (on {host})"),
        }
    }

    fn command(&self) -> Command {
        match &self.place {
            Place::Local(dir) => {
                let mut command = Command::new("git");
                command
                    .arg("-C")
                    .arg(dir)
                    .args(&self.args)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("GIT_OPTIONAL_LOCKS", "0");
                command
            }
            Place::Ssh { host, dir } => {
                let words = self.args.iter().map(|arg| arg.to_string_lossy());
                let mut line = String::from("GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0 git -C ");
                line.push_str(&crate::shell::quote(dir));
                for word in words {
                    line.push(' ');
                    line.push_str(&crate::shell::quote(&word));
                }
                let mut command = Command::new("ssh");
                // BatchMode: a missing key or an unknown host key fails
                // instead of waiting on a prompt nobody sees.
                command
                    .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=20"])
                    .arg(host)
                    .arg("--")
                    .arg(line);
                command
            }
        }
    }

    /// Runs git and returns its output whatever the exit status.
    pub fn run(mut self) -> Result<GitOutput> {
        let mut command = self.command();
        command
            .stdin(Stdio::null())
            .stdout(if self.capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());
        no_console_window(&mut command);

        let mut child = command.spawn()?;
        let stdout = child.stdout.take().map(|mut pipe| {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let mut out = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let room = STDOUT_LIMIT.saturating_sub(out.len());
                    out.extend_from_slice(&buf[..n.min(room)]);
                }
                let _ = tx.send(out);
            });
            rx
        });
        let tail = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = mpsc::channel();

        let stderr_pipe = child.stderr.take().expect("stderr is piped");
        let tail_sink = Arc::clone(&tail);
        let on_line = self.on_stderr_line.take();
        thread::spawn(move || {
            read_progress(stderr_pipe, &tail_sink, on_line);
            let _ = done_tx.send(());
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let failure = if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                Error::GitCanceled {
                    args: self.display_args(),
                }
            } else if Instant::now() >= deadline {
                Error::GitTimedOut {
                    args: self.display_args(),
                    timeout: self.timeout,
                }
            } else {
                thread::sleep(POLL);
                continue;
            };
            kill_tree(&mut child);
            return Err(failure);
        };
        let _ = done_rx.recv_timeout(DRAIN);
        let stderr = String::from_utf8_lossy(&lock(&tail)).trim().to_owned();
        let stdout = stdout
            .and_then(|reader| reader.recv_timeout(DRAIN).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        Ok(GitOutput {
            status,
            stderr,
            stdout,
        })
    }

    /// Runs git and fails unless it exits with status 0.
    pub fn run_checked(self) -> Result<GitOutput> {
        let args = self.display_args();
        let output = self.run()?;
        if output.status.success() {
            return Ok(output);
        }
        Err(Error::GitFailed {
            args,
            status: output.status.to_string(),
            stderr: output.stderr,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Splits stderr on `\r` and `\n` (git redraws progress with `\r`) and keeps
/// the last few KiB for error messages.
fn read_progress(mut pipe: impl Read, tail: &Mutex<Vec<u8>>, mut on_line: Option<LineSink>) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    let mut emit = |line: &mut Vec<u8>| {
        if let Some(sink) = on_line.as_mut()
            && !line.is_empty()
        {
            sink(String::from_utf8_lossy(line).trim_end());
        }
        line.clear();
    };
    while let Ok(n) = pipe.read(&mut buf) {
        if n == 0 {
            break;
        }
        {
            let mut tail = lock(tail);
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > STDERR_TAIL {
                let excess = tail.len() - STDERR_TAIL;
                tail.drain(..excess);
            }
        }
        for &byte in &buf[..n] {
            if byte == b'\r' || byte == b'\n' {
                emit(&mut line);
            } else {
                line.push(byte);
            }
        }
    }
    emit(&mut line);
}

#[cfg(windows)]
fn no_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_: &mut Command) {}

/// Kills git and what it spawned (ssh, credential helpers).
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID"])
            .arg(child.id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_console_window(&mut taskkill);
        let _ = taskkill.status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
