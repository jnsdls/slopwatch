//! The GUI's link to the daemon: connect, say hello, subscribe to the
//! topics the window shows, send the developer's commands, notice when the
//! daemon goes away, and hand off to a new daemon after an update. Blocking
//! I/O on a thread of its own, because GPUI's executors don't drive tokio
//! sockets.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use slopwatch_protocol::{
    Actor, ClientFrame, ClientHello, Command, LOCAL_URL, Refusal, RefusalReason, Request,
    RequestId, Response, ResponseBody, ServerFrame, ServerHello, Topic, TopicUpdate,
};
use tungstenite::{Message, WebSocket};

use crate::agent::{Agent, AgentStatus};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a read waits before the link checks for commands to send.
const COMMAND_POLL: Duration = Duration::from_millis(50);

/// What the window shows about the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Connected {
        daemon_build_id: String,
    },
    /// The daemon runs another build and no handoff can fix it: the GUI
    /// runs outside its bundle, or it already handed off this launch.
    Mismatched {
        daemon_build_id: String,
    },
    /// The GUI is replacing the daemon: `restart`, then `unregister` and
    /// `register` (ADR 0009).
    HandingOff,
    /// Nothing listens on the socket. `agent` is `None` when the GUI runs
    /// outside its bundle and has no agent to recover.
    NotRunning {
        agent: Option<AgentStatus>,
    },
    /// The daemon answered and turned the connection down.
    Refused {
        message: String,
    },
    /// Something answered on the socket but the handshake broke.
    Failed {
        message: String,
    },
}

/// Something the link reports to the window.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    State(LinkState),
    /// An update on a subscribed topic. Each connection starts with a fresh
    /// snapshot.
    Topic(TopicUpdate),
    /// The daemon's answer to one of the developer's commands.
    Response(Response),
}

#[derive(Debug)]
pub enum ConnectError {
    NotRunning(io::Error),
    Refused(Refusal),
    Failed(String),
}

impl From<ConnectError> for LinkState {
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::NotRunning(_) => LinkState::NotRunning { agent: None },
            ConnectError::Refused(refusal) => LinkState::Refused {
                message: refusal.message,
            },
            ConnectError::Failed(message) => LinkState::Failed { message },
        }
    }
}

/// An admitted connection to the daemon.
pub struct Session {
    ws: WebSocket<UnixStream>,
    daemon: ServerHello,
    next_id: u64,
}

impl Session {
    /// The daemon's hello.
    pub fn daemon(&self) -> &ServerHello {
        &self.daemon
    }

    /// Sends `command` as the developer through the GUI.
    pub fn send(&mut self, command: Command) -> tungstenite::Result<RequestId> {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        let request = ClientFrame::Request(Request {
            id,
            actor: Actor::Developer {
                via: "gui".to_owned(),
            },
            command,
        });
        let text = serde_json::to_string(&request).expect("client frames always serialize");
        self.ws.send(Message::text(text))?;
        Ok(id)
    }

    /// Asks the daemon to restart and waits until it answers or hangs up.
    /// The daemon exits right after, so the caller just reconnects.
    pub fn restart(mut self) -> Result<(), String> {
        self.ws
            .get_ref()
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|error| error.to_string())?;
        let id = self
            .send(Command::Restart)
            .map_err(|error| error.to_string())?;
        loop {
            match self.ws.read() {
                Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                    Ok(ServerFrame::Response(response)) if response.id == id => {
                        return match response.result {
                            ResponseBody::Ok(_) => Ok(()),
                            ResponseBody::Error(error) => Err(error.message),
                        };
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(());
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    /// The next frame, or `None` if none arrived within the read timeout.
    /// An error means the connection is gone.
    fn next_frame(&mut self) -> tungstenite::Result<Option<ServerFrame>> {
        match self.ws.read() {
            Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                Ok(frame) => Ok(Some(frame)),
                // A frame this build doesn't know. Additive changes are
                // feature strings, so skipping it is safe.
                Err(_) => Ok(None),
            },
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// Connects to the daemon on `path` and exchanges hellos.
pub fn connect(path: &Path, hello: &ClientHello) -> Result<Session, ConnectError> {
    let stream = UnixStream::connect(path).map_err(ConnectError::NotRunning)?;
    // A hung peer mustn't hold the link in the handshake forever.
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(failed)?;
    let (mut ws, _) = tungstenite::client(LOCAL_URL, stream).map_err(failed)?;

    let hello = serde_json::to_string(&ClientFrame::Hello(hello.clone()))
        .expect("client frames always serialize");
    ws.send(Message::text(hello)).map_err(failed)?;

    loop {
        let Message::Text(text) = ws.read().map_err(failed)? else {
            continue;
        };
        return match serde_json::from_str(&text).map_err(failed)? {
            ServerFrame::Hello(daemon) => Ok(Session {
                ws,
                daemon,
                next_id: 1,
            }),
            ServerFrame::Refused(refusal) => Err(ConnectError::Refused(refusal)),
            ServerFrame::Response(_) | ServerFrame::Topic(_) => Err(ConnectError::Failed(
                "The daemon answered the hello with something else.".to_owned(),
            )),
        };
    }
}

fn failed(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(error.to_string())
}

/// How the link paces itself.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// Between connection attempts while the daemon is down.
    pub retry: Duration,
    /// How long a re-registered daemon gets to answer before the GUI
    /// registers it again.
    pub handoff_wait: Duration,
}

impl Pace {
    pub const GUI: Pace = Pace {
        retry: Duration::from_secs(1),
        handoff_wait: Duration::from_secs(20),
    };
}

/// How many times one handoff may register the agent. A build with no Team
/// ID, such as an ad-hoc one, changes code identity on every rebuild, and
/// macOS keeps the agent's Background Task Management record pinned to the
/// old identity: the first `register` after a rebuild launches nothing.
/// About 10 s later macOS replaces the record, and a second `register`
/// starts the new daemon (ADR 0009). Signing with an Apple Development
/// identity, which has a Team ID, should retire the second register.
const REGISTERS_PER_HANDOFF: u32 = 2;

enum Wait {
    NotRegistering,
    /// launchd may still be starting the re-registered daemon.
    Starting,
    /// The re-registered daemon should have answered by now.
    Overdue,
}

/// The one handoff a GUI launch may run, so a daemon that keeps coming back
/// on another build can't loop it.
struct Handoff<'a> {
    agent: Option<&'a dyn Agent>,
    registers_left: u32,
    registered_at: Option<Instant>,
}

impl<'a> Handoff<'a> {
    fn new(agent: Option<&'a dyn Agent>) -> Self {
        Self {
            agent,
            registers_left: if agent.is_some() {
                REGISTERS_PER_HANDOFF
            } else {
                0
            },
            registered_at: None,
        }
    }

    fn can_register(&self) -> bool {
        self.registers_left > 0
    }

    /// Unregisters and registers the agent, so launchd starts the binary
    /// the bundle holds now.
    fn register(&mut self) {
        let Some(agent) = self.agent.filter(|_| self.can_register()) else {
            return;
        };
        self.registers_left -= 1;
        if let Err(error) = agent.reregister() {
            eprintln!("slopwatch: re-registering the daemon failed: {error}");
        }
        self.registered_at = Some(Instant::now());
    }

    /// Where a daemon that isn't up stands against the last register.
    fn wait(&self, patience: Duration) -> Wait {
        match self.registered_at {
            None => Wait::NotRegistering,
            Some(at) if at.elapsed() < patience => Wait::Starting,
            Some(_) => Wait::Overdue,
        }
    }

    /// Whether this launch hasn't registered or connected yet.
    fn untouched(&self) -> bool {
        self.registers_left == REGISTERS_PER_HANDOFF && self.registered_at.is_none()
    }

    fn stop_waiting(&mut self) {
        self.registered_at = None;
    }

    fn finish(&mut self) {
        self.registers_left = 0;
        self.registered_at = None;
    }
}

/// What the window hands the link: the developer's commands for the
/// daemon, and Re-register presses.
#[derive(Clone, Copy)]
pub struct Controls<'a> {
    pub commands: &'a Receiver<Command>,
    pub reregister: &'a Receiver<()>,
}

/// Keeps a link to the daemon on `path` as this build's client. Each
/// connection subscribes to `watched_prs` and `inbox`, and sends the
/// commands that arrive on `controls.commands`. Commands sent while the
/// daemon is down are dropped. Returns once `report` returns false.
///
/// A daemon from another build, or another dialect, gets replaced through
/// `agent`: `restart`, then `unregister` and `register` (ADR 0009), and
/// once more if the new daemon hasn't answered within
/// [`Pace::handoff_wait`]. With no `agent`, as under `cargo run`, the link
/// only reports the mismatch. A daemon that isn't running when the link
/// starts, and each Re-register press, get the same unregister and register
/// steps.
pub fn run(
    path: &Path,
    pace: Pace,
    agent: Option<&dyn Agent>,
    controls: Controls,
    mut report: impl FnMut(LinkEvent) -> bool,
) {
    let hello = ClientHello::local();
    let mut last = LinkState::Connecting;
    // Reports a state only when it differs from the last one.
    let mut changed = |state: LinkState, report: &mut dyn FnMut(LinkEvent) -> bool| {
        state == last || {
            last = state.clone();
            report(LinkEvent::State(state))
        }
    };
    let not_running = || LinkState::NotRunning {
        agent: agent.map(|agent| agent.status()),
    };
    let mut handoff = Handoff::new(agent);

    loop {
        while controls.commands.try_recv().is_ok() {}
        match connect(path, &hello) {
            Ok(session) if session.daemon().build_id == hello.build_id => {
                handoff.finish();
                let connected = LinkState::Connected {
                    daemon_build_id: session.daemon().build_id.clone(),
                };
                if !changed(connected, &mut report) || !serve(session, controls, &mut report) {
                    return;
                }
                if !changed(not_running(), &mut report) {
                    return;
                }
            }
            Ok(session) if handoff.can_register() => {
                if !changed(LinkState::HandingOff, &mut report) {
                    return;
                }
                // A daemon that can't answer `restart` still goes away
                // when it's unregistered.
                if let Err(error) = session.restart() {
                    eprintln!("slopwatch: restart failed: {error}");
                }
                handoff.register();
                continue;
            }
            Ok(session) => {
                handoff.finish();
                let mismatched = LinkState::Mismatched {
                    daemon_build_id: session.daemon().build_id.clone(),
                };
                if !changed(mismatched, &mut report) || !serve(session, controls, &mut report) {
                    return;
                }
            }
            Err(ConnectError::Refused(refusal))
                if refusal.reason == RefusalReason::DialectMismatch && handoff.can_register() =>
            {
                // The daemon won't take requests from another dialect, so it
                // can't hear `restart`. Unregistering stops it instead.
                if !changed(LinkState::HandingOff, &mut report) {
                    return;
                }
                handoff.register();
                continue;
            }
            Err(ConnectError::NotRunning(_)) => match handoff.wait(pace.handoff_wait) {
                Wait::Starting => {}
                Wait::Overdue if handoff.can_register() => {
                    handoff.register();
                    continue;
                }
                // Nothing answers at launch: a first install, or an agent
                // macOS lost track of. Registering starts it, unless the
                // developer turned it off in Login Items.
                Wait::NotRegistering
                    if handoff.untouched()
                        && agent.is_some_and(|agent| {
                            agent.status() != AgentStatus::RequiresApproval
                        }) =>
                {
                    if !changed(LinkState::HandingOff, &mut report) {
                        return;
                    }
                    handoff.register();
                    continue;
                }
                Wait::Overdue | Wait::NotRegistering => {
                    handoff.stop_waiting();
                    if !changed(not_running(), &mut report) {
                        return;
                    }
                }
            },
            // The old daemon may still be going away.
            Err(_) if matches!(handoff.wait(pace.handoff_wait), Wait::Starting) => {}
            Err(error) => {
                if !changed(error.into(), &mut report) {
                    return;
                }
            }
        }
        if requested(controls.reregister, pace.retry) {
            // The developer asked for it, so this launch gets a fresh handoff.
            handoff = Handoff::new(agent);
            if !changed(LinkState::HandingOff, &mut report) {
                return;
            }
            handoff.register();
        }
    }
}

/// Waits `retry` for a Re-register press. True if one came.
fn requested(reregister: &Receiver<()>, retry: Duration) -> bool {
    match reregister.recv_timeout(retry) {
        Ok(()) => true,
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => {
            std::thread::sleep(retry);
            false
        }
    }
}

/// Runs one connection until it drops. Returns false if `report` asked to
/// stop.
fn serve(
    mut session: Session,
    controls: Controls,
    report: &mut dyn FnMut(LinkEvent) -> bool,
) -> bool {
    let commands = controls.commands;
    let subscribed = [Topic::WatchedPrs, Topic::Inbox].into_iter().all(|topic| {
        session
            .send(Command::Subscribe { topic, since: None })
            .is_ok()
    });
    if !subscribed
        || session
            .ws
            .get_ref()
            .set_read_timeout(Some(COMMAND_POLL))
            .is_err()
    {
        return true;
    }
    loop {
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    if session.send(command).is_err() {
                        return true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        let event = match session.next_frame() {
            Ok(Some(ServerFrame::Topic(update))) => LinkEvent::Topic(update),
            Ok(Some(ServerFrame::Response(response))) => LinkEvent::Response(response),
            Ok(Some(ServerFrame::Hello(_) | ServerFrame::Refused(_)) | None) => continue,
            Err(_) => return true,
        };
        if !report(event) {
            return false;
        }
    }
}
