//! The GUI's link to the daemon: connect, say hello, subscribe to the
//! topics the window shows, send the developer's commands, and notice when
//! the daemon goes away. Blocking I/O on a thread of its own, because
//! GPUI's executors don't drive tokio sockets.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use slopwatch_protocol::{
    Actor, ClientFrame, ClientHello, Command, LOCAL_URL, Refusal, Request, RequestId, Response,
    ServerFrame, ServerHello, Topic, TopicUpdate,
};
use tungstenite::{Message, WebSocket};

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
    /// Nothing listens on the socket.
    NotRunning,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            ConnectError::NotRunning(_) => LinkState::NotRunning,
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

/// Keeps a link to the daemon on `path`, retrying every `retry` while it's
/// down. Each connection subscribes to `watched_prs` and sends the commands
/// that arrive on `commands`. Commands sent while the daemon is down are
/// dropped. Returns once `report` returns false.
pub fn run(
    path: &Path,
    retry: Duration,
    commands: &Receiver<Command>,
    mut report: impl FnMut(LinkEvent) -> bool,
) {
    let mut last = LinkState::Connecting;
    // Reports a state only when it differs from the last one.
    let mut changed = |state: LinkState, report: &mut dyn FnMut(LinkEvent) -> bool| {
        state == last || {
            last = state.clone();
            report(LinkEvent::State(state))
        }
    };

    loop {
        while commands.try_recv().is_ok() {}
        match connect(path, &ClientHello::local()) {
            Ok(session) => {
                let connected = LinkState::Connected {
                    daemon_build_id: session.daemon().build_id.clone(),
                };
                if !changed(connected, &mut report) {
                    return;
                }
                if !serve(session, commands, &mut report) {
                    return;
                }
                if !changed(LinkState::NotRunning, &mut report) {
                    return;
                }
            }
            Err(error) => {
                if !changed(error.into(), &mut report) {
                    return;
                }
            }
        }
        std::thread::sleep(retry);
    }
}

/// Runs one connection until it drops. Returns false if `report` asked to
/// stop.
fn serve(
    mut session: Session,
    commands: &Receiver<Command>,
    report: &mut dyn FnMut(LinkEvent) -> bool,
) -> bool {
    let subscribe = Command::Subscribe {
        topic: Topic::WatchedPrs,
    };
    if session.send(subscribe).is_err()
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
