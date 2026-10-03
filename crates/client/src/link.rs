//! The GUI's link to the daemon: connect, say hello, notice when it goes
//! away. Blocking I/O on a thread of its own, because GPUI's executors
//! don't drive tokio sockets.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use slopwatch_protocol::{ClientFrame, ClientHello, LOCAL_URL, Refusal, ServerFrame, ServerHello};
use tungstenite::{Message, WebSocket};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
}

impl Session {
    /// The daemon's hello.
    pub fn daemon(&self) -> &ServerHello {
        &self.daemon
    }

    /// Blocks until the daemon closes the connection or goes away.
    pub fn wait_closed(mut self) {
        while self.ws.read().is_ok() {}
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
            ServerFrame::Hello(daemon) => {
                ws.get_ref().set_read_timeout(None).map_err(failed)?;
                Ok(Session { ws, daemon })
            }
            ServerFrame::Refused(refusal) => Err(ConnectError::Refused(refusal)),
            ServerFrame::Response(_) => Err(ConnectError::Failed(
                "The daemon answered the hello with a response.".to_owned(),
            )),
        };
    }
}

fn failed(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(error.to_string())
}

/// Keeps a link to the daemon on `path` and reports each state change,
/// retrying every `retry` while it's down. Returns once `report` returns
/// false.
pub fn watch(path: &Path, retry: Duration, mut report: impl FnMut(LinkState) -> bool) {
    let mut last = LinkState::Connecting;
    let mut changed = |state: LinkState| {
        if state == last {
            return true;
        }
        last = state.clone();
        report(state)
    };

    loop {
        match connect(path, &ClientHello::local()) {
            Ok(session) => {
                let connected = LinkState::Connected {
                    daemon_build_id: session.daemon().build_id.clone(),
                };
                if !changed(connected) {
                    return;
                }
                session.wait_closed();
                if !changed(LinkState::NotRunning) {
                    return;
                }
            }
            Err(error) => {
                if !changed(error.into()) {
                    return;
                }
            }
        }
        std::thread::sleep(retry);
    }
}
