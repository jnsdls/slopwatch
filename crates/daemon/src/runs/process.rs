//! One Step process: spawned in its own process group, spoken to in JSONL
//! over stdio, cancelled the Step contract's way.
//!
//! A task owns the child. It forwards what the Step says as [`Report`]s and
//! takes [`Control`]s from the engine. Cancel sends `cancel`, then SIGTERM
//! to the group 10 s later and SIGKILL 5 s after that. Stderr and the
//! Step's `log` messages go to the Step log file.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use slopwatch_protocol::step::{FromStep, ToStep};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::Sleep;

/// After `cancel`, how long a Step gets before SIGTERM, then before SIGKILL.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Everything needed to start one Step process.
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The Step's working directory.
    pub dir: PathBuf,
    /// The whole environment: nothing else leaks in from the daemon's.
    pub env: Vec<(String, String)>,
    pub log: PathBuf,
    pub start: ToStep,
}

/// What a running Step process reports.
#[derive(Debug)]
pub enum Report {
    Message(FromStep),
    /// A stdout line that isn't a protocol message.
    ProtocolError(String),
    /// The process exited. `None` when a signal ended it.
    Exited(Option<i32>),
}

#[derive(Debug)]
pub enum Control {
    Send(Box<ToStep>),
    Cancel,
}

/// The engine's handle on a running Step. Dropping it cancels the Step.
pub struct StepHandle {
    control: mpsc::UnboundedSender<Control>,
    pub pgid: i32,
    /// When the kernel says the process started, in microseconds since the
    /// Unix epoch.
    pub started_us: i64,
}

impl StepHandle {
    pub fn send(&self, message: ToStep) {
        // A process that already exited has nothing left to hear.
        let _ = self.control.send(Control::Send(Box::new(message)));
    }

    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    /// SIGKILLs the Step's group now, with no cancel first.
    pub fn kill(&self) {
        signal_group(self.pgid, libc::SIGKILL);
    }
}

/// Starts the process and its task. `report` gets everything the process
/// says, last of all its exit.
pub fn spawn(spawn: Spawn, report: impl Fn(Report) + Send + 'static) -> io::Result<StepHandle> {
    if let Some(parent) = spawn.log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spawn.log)?;
    let mut child = Command::new(&spawn.program)
        .args(&spawn.args)
        .current_dir(&spawn.dir)
        .env_clear()
        .envs(spawn.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log.try_clone()?))
        // Its own group, so cancelling reaches whatever it starts.
        .process_group(0)
        .kill_on_drop(false)
        .spawn()?;
    let pid = child.id().expect("a just-spawned child has a pid") as i32;
    let started_us = process_start_us(pid);
    let (control, controls) = mpsc::unbounded_channel();
    tokio::spawn(session(
        child_parts(&mut child),
        child,
        pid,
        spawn.start,
        controls,
        log,
        report,
    ));
    Ok(StepHandle {
        control,
        pgid: pid,
        started_us,
    })
}

struct Parts {
    stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
}

fn child_parts(child: &mut Child) -> Parts {
    Parts {
        stdin: child.stdin.take().expect("stdin is piped"),
        stdout: child.stdout.take().expect("stdout is piped"),
    }
}

async fn session(
    parts: Parts,
    mut child: Child,
    pgid: i32,
    start: ToStep,
    mut controls: mpsc::UnboundedReceiver<Control>,
    log: std::fs::File,
    report: impl Fn(Report),
) {
    let mut log = tokio::fs::File::from_std(log);
    let mut stdin = Some(parts.stdin);
    let mut lines = BufReader::new(parts.stdout).lines();
    let mut stdout_open = true;
    let mut controls_open = true;
    // The next signal to send to the group, and when.
    let mut kill: Option<(libc::c_int, Pin<Box<Sleep>>)> = None;

    write_message(&mut stdin, &start).await;
    loop {
        tokio::select! {
            line = lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => match serde_json::from_str::<FromStep>(&line) {
                    Ok(FromStep::Log { message }) => {
                        let _ = log.write_all(format!("{message}\n").as_bytes()).await;
                    }
                    Ok(message) => report(Report::Message(message)),
                    Err(error) => report(Report::ProtocolError(format!(
                        "wrote a line that isn't a protocol message ({error}): {}",
                        truncate(&line)
                    ))),
                },
                Ok(None) | Err(_) => stdout_open = false,
            },
            control = controls.recv(), if controls_open => match control {
                Some(Control::Send(message)) => write_message(&mut stdin, &message).await,
                Some(Control::Cancel) | None => {
                    controls_open = control.is_some();
                    if kill.is_none() {
                        write_message(&mut stdin, &ToStep::Cancel).await;
                        kill = Some((libc::SIGTERM, Box::pin(tokio::time::sleep(CANCEL_GRACE))));
                    }
                }
            },
            () = async { kill.as_mut().expect("guarded").1.as_mut().await }, if kill.is_some() => {
                let (signal, _) = kill.take().expect("guarded");
                signal_group(pgid, signal);
                if signal == libc::SIGTERM {
                    kill = Some((libc::SIGKILL, Box::pin(tokio::time::sleep(TERM_GRACE))));
                }
            },
            status = child.wait() => {
                // Read what's left on stdout: an outcome may still be there.
                while stdout_open {
                    match lines.next_line().await {
                        Ok(Some(line)) => match serde_json::from_str::<FromStep>(&line) {
                            Ok(FromStep::Log { message }) => {
                                let _ = log.write_all(format!("{message}\n").as_bytes()).await;
                            }
                            Ok(message) => report(Report::Message(message)),
                            Err(error) => report(Report::ProtocolError(format!(
                                "wrote a line that isn't a protocol message ({error}): {}",
                                truncate(&line)
                            ))),
                        },
                        _ => stdout_open = false,
                    }
                }
                // Whatever the Step started in its group goes with it.
                signal_group(pgid, libc::SIGKILL);
                report(Report::Exited(status.ok().and_then(|status| status.code())));
                return;
            }
        }
    }
}

async fn write_message(stdin: &mut Option<ChildStdin>, message: &ToStep) {
    let Some(pipe) = stdin else { return };
    let mut line = serde_json::to_string(message).expect("Step messages always serialize");
    line.push('\n');
    if write_line(pipe, &line).await.is_err() {
        // The Step closed its stdin or exited. Its exit reports the rest.
        *stdin = None;
    }
}

async fn write_line(pipe: &mut (impl AsyncWrite + Unpin), line: &str) -> io::Result<()> {
    pipe.write_all(line.as_bytes()).await?;
    pipe.flush().await
}

fn truncate(line: &str) -> String {
    const MAX: usize = 200;
    match line.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}

fn signal_group(pgid: i32, signal: libc::c_int) {
    // SAFETY: killpg takes plain integers and touches no memory. A group
    // that's already gone just returns ESRCH.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// When the kernel says `pid` started, in microseconds since the Unix
/// epoch, or now if it can't say.
fn process_start_us(pid: i32) -> i64 {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is a proc_bsdinfo of exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if written == size {
            return (info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec) as i64;
        }
    }
    let _ = pid;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_micros() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, dir: &tempfile::TempDir) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            dir: dir.path().to_owned(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            log: dir.path().join("logs/step.log"),
            start: ToStep::Cancel,
        }
    }

    async fn reports(spawn_with: Spawn) -> (StepHandle, mpsc::UnboundedReceiver<Report>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let handle = spawn(spawn_with, move |report| {
            let _ = sender.send(report);
        })
        .unwrap();
        (handle, receiver)
    }

    #[tokio::test]
    async fn a_step_runs_in_its_own_process_group_with_only_the_env_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"read line; printf '{"type":"progress","message":"%s %s %s"}\n' "$(ps -o pgid= -p $$ | tr -d ' ')" "$$" "${HOME:-nohome}""#;
        let (handle, mut reports) = reports(sh(script, &dir)).await;

        let Some(Report::Message(FromStep::Progress {
            message: Some(message),
        })) = reports.recv().await
        else {
            panic!("expected a progress message");
        };
        let parts: Vec<&str> = message.split(' ').collect();
        assert_eq!(parts[0], parts[1], "the Step leads its own group");
        assert_eq!(parts[0], handle.pgid.to_string());
        assert_eq!(parts[2], "nohome", "the daemon's env doesn't leak in");
        assert!(handle.started_us > 0);
        assert!(matches!(
            reports.recv().await,
            Some(Report::Exited(Some(0)))
        ));
    }

    #[tokio::test]
    async fn stderr_and_log_messages_go_to_the_step_log() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"echo to stderr >&2; echo '{"type":"log","message":"logged"}'"#;
        let (_handle, mut reports) = reports(sh(script, &dir)).await;

        assert!(matches!(
            reports.recv().await,
            Some(Report::Exited(Some(0)))
        ));
        let log = std::fs::read_to_string(dir.path().join("logs/step.log")).unwrap();
        assert!(log.contains("to stderr"), "{log}");
        assert!(log.contains("logged"), "{log}");
    }

    #[tokio::test]
    async fn a_non_protocol_line_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, mut reports) = reports(sh("echo hello", &dir)).await;

        assert!(matches!(
            reports.recv().await,
            Some(Report::ProtocolError(error)) if error.contains("hello")
        ));
    }

    #[tokio::test]
    async fn cancel_tells_the_step_and_it_can_stop_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        // The test's start message is a cancel too, so the second line counts.
        let script = r#"n=0; while read line; do n=$((n+1)); echo "$line" >> heard; [ $n -ge 2 ] && exit 3; done"#;
        let (handle, mut reports) = reports(sh(script, &dir)).await;

        handle.cancel();

        assert!(matches!(
            reports.recv().await,
            Some(Report::Exited(Some(3)))
        ));
        let heard = std::fs::read_to_string(dir.path().join("heard")).unwrap();
        assert_eq!(heard.lines().count(), 2, "start, then cancel: {heard}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_ignores_cancel_gets_terminated() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, mut reports) =
            reports(sh("trap '' INT; while true; do sleep 1; done", &dir)).await;

        handle.cancel();

        // The paused clock jumps to the SIGTERM deadline once idle.
        assert!(matches!(reports.recv().await, Some(Report::Exited(None))));
    }
}
