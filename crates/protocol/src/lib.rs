//! The wire contract between slopwatch's daemon and its clients.
//!
//! Clients speak WebSocket with one JSON object per text frame, over the
//! daemon's Unix socket in v1 (ADR 0010). The first frame each side sends is
//! a hello. After that the client sends requests and the daemon answers each
//! one with a response carrying the same id.

mod frames;
mod socket;

pub use frames::{
    Actor, Auth, ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, Refusal, RefusalReason,
    Reply, Request, RequestId, Response, ResponseBody, ServerFrame, ServerHello,
};
pub use socket::{LOCAL_URL, SOCKET_ENV, local_socket_path};

/// The protocol dialect. Both sides must speak exactly the same one. Any
/// change an older peer can't ignore bumps it, and additive changes become
/// feature strings instead.
pub const DIALECT: u32 = 1;

/// The git SHA this binary was built from, plus a hash of the uncommitted
/// changes when the tree was dirty.
pub const BUILD_ID: &str = env!("SLOPWATCH_BUILD_ID");
