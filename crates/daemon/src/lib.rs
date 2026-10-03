//! The slopwatch daemon.
//!
//! [`Daemon::serve`] serves one client connection over any byte stream, and
//! a transport runs one per connected client. The [`transport`] module feeds
//! it streams: [`transport::unix`] from the
//! Unix socket, and [`transport::in_process`] from an in-memory duplex that
//! carries the same WebSocket frames, for tests.

mod connection;
pub mod transport;

use slopwatch_protocol::{
    Auth, BUILD_ID, ClientFrame, ClientHello, Command, DIALECT, Refusal, RefusalReason, Reply,
    ServerHello,
};

/// Who is on the other end of a connection, as the transport reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
}

pub struct Daemon {
    build_id: String,
    uid: u32,
}

impl Daemon {
    /// A daemon stamped with this binary's build id, serving peers that run
    /// as the current user.
    pub fn new() -> Self {
        Self::with_build_id(BUILD_ID)
    }

    pub fn with_build_id(build_id: impl Into<String>) -> Self {
        Self {
            build_id: build_id.into(),
            uid: current_uid(),
        }
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    /// The uid this daemon runs as. `local` auth admits only peers with the
    /// same uid.
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Answers a client's first frame. A stranger learns nothing about the
    /// daemon, and the dialect is read before the rest of the hello, so a
    /// client from another dialect gets the restart hint even if its hello
    /// changed shape.
    fn admit(&self, first_frame: &str, peer: Peer) -> Result<ServerHello, Refusal> {
        if peer.uid != self.uid {
            return Err(Refusal {
                reason: RefusalReason::PeerUidMismatch,
                message: "The daemon serves only the user it runs as.".to_owned(),
            });
        }
        let not_a_hello = |detail: String| Refusal {
            reason: RefusalReason::ExpectedHello,
            message: format!("The first frame must be a hello. {detail}")
                .trim_end()
                .to_owned(),
        };
        let dialect =
            ClientHello::peek_dialect(first_frame).ok_or_else(|| not_a_hello(String::new()))?;
        if dialect != DIALECT {
            return Err(Refusal {
                reason: RefusalReason::DialectMismatch,
                message: format!(
                    "The daemon speaks protocol dialect {DIALECT} and this client speaks \
                     {dialect}. Restart the daemon so both run the same build."
                ),
            });
        }
        let hello = match serde_json::from_str(first_frame) {
            Ok(ClientFrame::Hello(hello)) => hello,
            Ok(ClientFrame::Request(_)) => return Err(not_a_hello(String::new())),
            Err(error) => return Err(not_a_hello(error.to_string())),
        };
        match hello.auth {
            Auth::Local => Ok(ServerHello {
                dialect: DIALECT,
                features: Vec::new(),
                build_id: self.build_id.clone(),
            }),
            Auth::Unsupported => Err(Refusal {
                reason: RefusalReason::UnsupportedAuth,
                message: "This daemon accepts only local auth.".to_owned(),
            }),
        }
    }

    fn execute(&self, command: Command) -> Reply {
        match command {
            Command::Ping => Reply::Pong,
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments, always succeeds and touches no memory.
    unsafe { libc::getuid() }
}
