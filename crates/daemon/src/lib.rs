//! The slopwatch daemon.
//!
//! [`Daemon`] serves one client connection at a time over any byte stream.
//! The [`transport`] module feeds it streams: [`transport::unix`] from the
//! Unix socket, and [`transport::in_process`] from an in-memory duplex that
//! carries the same WebSocket frames, for tests.

mod connection;
pub mod transport;

use slopwatch_protocol::{
    Auth, BUILD_ID, ClientHello, Command, DIALECT, Refusal, RefusalReason, Reply, ServerHello,
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

    fn admit(&self, hello: &ClientHello, peer: Peer) -> Result<ServerHello, Refusal> {
        if hello.dialect != DIALECT {
            return Err(Refusal {
                reason: RefusalReason::DialectMismatch,
                message: format!(
                    "The daemon speaks protocol dialect {DIALECT} and this client speaks {}. \
                     Restart the daemon so both run the same build.",
                    hello.dialect
                ),
            });
        }
        match hello.auth {
            Auth::Local => {}
            Auth::Unsupported => {
                return Err(Refusal {
                    reason: RefusalReason::UnsupportedAuth,
                    message: "This daemon accepts only local auth.".to_owned(),
                });
            }
        }
        if peer.uid != self.uid {
            return Err(Refusal {
                reason: RefusalReason::PeerUidMismatch,
                message: format!(
                    "The daemon runs as uid {} and refuses clients running as uid {}.",
                    self.uid, peer.uid
                ),
            });
        }
        Ok(ServerHello {
            dialect: DIALECT,
            features: Vec::new(),
            build_id: self.build_id.clone(),
        })
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
