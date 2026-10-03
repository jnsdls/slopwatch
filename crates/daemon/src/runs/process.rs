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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slopwatch_protocol::step::{FromStep, ToStep};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::Sleep;

/// After `cancel`, how long a Step gets before SIGTERM, then before SIGKILL.
const CANCEL_GRACE: Duration = Duration::from_secs(10);
const TERM_GRACE: Duration = Duration::from_secs(5);
/// How long the rest of stdout gets once the process has exited.
const DRAIN: Duration = Duration::from_secs(2);

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
enum Control {
    Send(Box<ToStep>),
    Cancel,
}

/// The engine's handle on a running Step. Dropping it cancels the Step.
pub struct StepHandle {
    control: mpsc::UnboundedSender<Control>,
    /// Set once the process has exited and been reaped, after which its
    /// pid, and so its group id, may belong to someone else.
    exited: Arc<AtomicBool>,
    pub pgid: i32,
    /// When the kernel says the process started, in microseconds since the
    /// Unix epoch.
    pub started_us: i64,
    /// The session the Step runs in: the daemon's.
    pub sid: i32,
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
        if !self.exited.load(Ordering::Acquire) {
            signal_group(self.pgid, libc::SIGKILL);
        }
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
    let sid = session_of(pid);
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let (control, controls) = mpsc::unbounded_channel();
    let exited = Arc::new(AtomicBool::new(false));
    let session = Session {
        child,
        pgid: pid,
        exited: Arc::clone(&exited),
        to_step: writer(stdin),
        log: tokio::fs::File::from_std(log),
        report,
    };
    let _ = session.to_step.send(spawn.start);
    tokio::spawn(session.run(stdout, controls));
    Ok(StepHandle {
        control,
        exited,
        pgid: pid,
        started_us,
        sid,
    })
}

/// Writes messages to the Step's stdin on a task of its own, so a Step
/// that stops reading can't stall its session.
fn writer(mut stdin: ChildStdin) -> mpsc::UnboundedSender<ToStep> {
    let (sender, mut messages) = mpsc::unbounded_channel::<ToStep>();
    tokio::spawn(async move {
        while let Some(message) = messages.recv().await {
            let mut line = serde_json::to_string(&message).expect("Step messages always serialize");
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                // The Step closed its stdin or exited. Its exit reports
                // the rest.
                return;
            }
        }
    });
    sender
}

struct Session<R> {
    child: Child,
    pgid: i32,
    exited: Arc<AtomicBool>,
    to_step: mpsc::UnboundedSender<ToStep>,
    log: tokio::fs::File,
    report: R,
}

impl<R: Fn(Report)> Session<R> {
    async fn run(mut self, stdout: ChildStdout, mut controls: mpsc::UnboundedReceiver<Control>) {
        let mut lines = BufReader::new(stdout).lines();
        let mut stdout_open = true;
        let mut controls_open = true;
        // The next signal to send to the group, and when.
        let mut kill: Option<(libc::c_int, Pin<Box<Sleep>>)> = None;
        loop {
            tokio::select! {
                line = lines.next_line(), if stdout_open => match line {
                    Ok(Some(line)) => self.line(&line).await,
                    Ok(None) | Err(_) => stdout_open = false,
                },
                control = controls.recv(), if controls_open => match control {
                    Some(Control::Send(message)) => {
                        let _ = self.to_step.send(*message);
                    }
                    Some(Control::Cancel) | None => {
                        controls_open = control.is_some();
                        if kill.is_none() {
                            let _ = self.to_step.send(ToStep::Cancel);
                            kill = Some((libc::SIGTERM, Box::pin(tokio::time::sleep(CANCEL_GRACE))));
                        }
                    }
                },
                () = async { kill.as_mut().expect("guarded").1.as_mut().await }, if kill.is_some() => {
                    let (signal, _) = kill.take().expect("guarded");
                    signal_group(self.pgid, signal);
                    if signal == libc::SIGTERM {
                        kill = Some((libc::SIGKILL, Box::pin(tokio::time::sleep(TERM_GRACE))));
                    }
                },
                status = self.child.wait() => {
                    // Whatever the Step started in its group goes with it,
                    // which also closes any stdout it handed down.
                    signal_group(self.pgid, libc::SIGKILL);
                    self.exited.store(true, Ordering::Release);
                    // Read what's left on stdout: an outcome may be there.
                    let deadline = tokio::time::Instant::now() + DRAIN;
                    while stdout_open {
                        match tokio::time::timeout_at(deadline, lines.next_line()).await {
                            Ok(Ok(Some(line))) => self.line(&line).await,
                            _ => stdout_open = false,
                        }
                    }
                    (self.report)(Report::Exited(status.ok().and_then(|status| status.code())));
                    return;
                }
            }
        }
    }

    /// One stdout line: a log message goes to the Step log, and anything
    /// else to the engine.
    async fn line(&mut self, line: &str) {
        match serde_json::from_str::<FromStep>(line) {
            Ok(FromStep::Log { message }) => {
                // tokio's File hands writes to a blocking thread, and only
                // a flush waits for them to land.
                let line = format!("{message}\n");
                if self.log.write_all(line.as_bytes()).await.is_ok() {
                    let _ = self.log.flush().await;
                }
            }
            Ok(message) => (self.report)(Report::Message(message)),
            Err(error) => (self.report)(Report::ProtocolError(format!(
                "wrote a line that isn't a protocol message ({error}): {}",
                truncate(line)
            ))),
        }
    }
}

fn truncate(line: &str) -> String {
    const MAX: usize = 200;
    match line.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}

pub(crate) fn signal_group(pgid: i32, signal: libc::c_int) {
    // SAFETY: killpg takes plain integers and touches no memory. A group
    // that's already gone just returns ESRCH.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// When the kernel says `pid` started, in microseconds since the Unix
/// epoch, or now if it can't say.
fn process_start_us(pid: i32) -> i64 {
    kernel_start_us(pid).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_micros() as i64)
            .unwrap_or_default()
    })
}

/// When the kernel says `pid` started, if it's running.
pub(crate) fn kernel_start_us(pid: i32) -> Option<i64> {
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
            return Some((info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec) as i64);
        }
    }
    let _ = pid;
    None
}

/// A Step's process group as the store recorded it at spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leftover {
    pub pgid: i32,
    pub started_us: i64,
    pub sid: i32,
}

/// Kills a Step's group left over from before a restart (ADR 0009), once
/// it's clear the group is still the Step's and not a later one that
/// reused the pid. With the leader still running, its start time decides.
/// A leader that exited, as one may when the daemon's end closes its
/// stdin, can leave children in the group. Then every member must sit in
/// the session the Step ran in and have started no earlier than its
/// leader.
pub(crate) fn kill_leftover(leftover: &Leftover) {
    let Leftover {
        pgid,
        started_us,
        sid,
    } = *leftover;
    if pgid <= 0 {
        return;
    }
    let ours = match kernel_start_us(pgid) {
        Some(leader_started) => leader_started == started_us,
        None => {
            // Members that exited in the meantime have nothing to say.
            let members: Vec<(i64, i32)> = group_members(pgid)
                .into_iter()
                .filter_map(|pid| Some((kernel_start_us(pid)?, session_of(pid))))
                .collect();
            !members.is_empty()
                && members
                    .iter()
                    .all(|&(started, session)| started >= started_us && session == sid)
        }
    };
    if ours {
        signal_group(pgid, libc::SIGKILL);
    }
}

/// The session `pid` belongs to, or -1 if it's gone.
pub(crate) fn session_of(pid: i32) -> i32 {
    // SAFETY: getsid takes a plain integer and touches no memory.
    unsafe { libc::getsid(pid) }
}

/// The pids in process group `pgid`.
fn group_members(pgid: i32) -> Vec<i32> {
    #[cfg(target_os = "macos")]
    {
        let mut pids = vec![0_i32; 256];
        loop {
            let size = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
            // SAFETY: the buffer holds exactly `size` bytes of pids.
            let count = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
            if count < 0 {
                return Vec::new();
            }
            let count = count as usize;
            if count < pids.len() {
                pids.truncate(count);
                return pids.into_iter().filter(|&pid| pid > 0).collect();
            }
            pids.resize(pids.len() * 2, 0);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pgid;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt as _;

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

    #[tokio::test]
    async fn a_step_that_leaves_a_child_holding_stdout_still_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, mut reports) = reports(sh("sleep 600 & exit 0", &dir)).await;

        let exited = tokio::time::timeout(Duration::from_secs(10), reports.recv()).await;

        assert!(
            matches!(exited, Ok(Some(Report::Exited(Some(0))))),
            "{exited:?}"
        );
    }

    #[tokio::test]
    async fn a_leftover_group_dies_only_if_its_leader_started_when_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, mut reports) = reports(sh("read line; sleep 600", &dir)).await;

        kill_leftover(&leftover(&handle, |l| l.started_us += 1));
        let early = tokio::time::timeout(Duration::from_millis(300), reports.recv()).await;
        assert!(early.is_err(), "a different start time spares it");

        kill_leftover(&leftover(&handle, |_| {}));
        assert!(matches!(reports.recv().await, Some(Report::Exited(None))));
    }

    fn leftover(handle: &StepHandle, change: impl FnOnce(&mut Leftover)) -> Leftover {
        let mut leftover = Leftover {
            pgid: handle.pgid,
            started_us: handle.started_us,
            sid: handle.sid,
        };
        change(&mut leftover);
        leftover
    }

    /// A Step whose leader exited, as a Step might once a crashed daemon
    /// closes its stdin, leaving a child behind in its group. Returns the
    /// group as recorded at spawn and the child's pid.
    fn leaderless_group() -> (Leftover, i32) {
        let mut leader = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 600 & echo $!; read line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = leader.id() as i32;
        let recorded = Leftover {
            pgid,
            started_us: kernel_start_us(pgid).unwrap(),
            sid: session_of(pgid),
        };
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(leader.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let child: i32 = line.trim().parse().unwrap();
        drop(leader.stdin.take());
        leader.wait().unwrap();
        (recorded, child)
    }

    fn alive(pid: i32) -> bool {
        kernel_start_us(pid).is_some()
    }

    async fn until_gone(pid: i32) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_leftover_group_whose_leader_exited_still_dies() {
        let (recorded, child) = leaderless_group();
        assert!(alive(child), "the child outlives its leader");

        kill_leftover(&recorded);

        assert!(until_gone(child).await, "the leftover child was killed");
    }

    #[tokio::test]
    async fn a_leaderless_group_from_another_session_or_an_earlier_time_is_spared() {
        let (recorded, child) = leaderless_group();

        kill_leftover(&Leftover {
            sid: recorded.sid + 1,
            ..recorded
        });
        kill_leftover(&Leftover {
            started_us: kernel_start_us(child).unwrap() + 1,
            ..recorded
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let spared = alive(child);
        signal_group(recorded.pgid, libc::SIGKILL);

        assert!(spared, "nothing proves the group is the Step's");
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
