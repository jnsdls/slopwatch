//! The wire contract between slopwatch's daemon and its clients.
//!
//! Clients speak WebSocket with one JSON object per text frame, over the
//! daemon's Unix socket in v1 (ADR 0010). The first frame each side sends is
//! a hello. After that the client sends requests and the daemon answers each
//! one with a response carrying the same id. A client that subscribes to a
//! topic also gets topic updates: a snapshot, then ordered deltas.

mod flavor;
mod frames;
pub mod runs;
pub mod step;
mod topics;

pub use flavor::{DATA_DIR_ENV, Flavor, socket_path};
pub use frames::{
    Actor, Auth, ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, LibraryStep, Refusal,
    RefusalReason, Reply, Request, RequestId, Response, ResponseBody, ServerFrame, ServerHello,
};
pub use runs::{RunEvent, RunId, RunSummary, RunView, StepInfo, StepStatus, StepView};
pub use topics::{
    PollState, PrStatus, PullRequest, RepoName, Topic, TopicUpdate, WatchedPrs, WatchedPrsDelta,
    WatchedPrsUpdate,
};

/// The protocol dialect. Both sides must speak exactly the same one. Any
/// change an older peer can't ignore bumps it, and additive changes become
/// feature strings instead.
pub const DIALECT: u32 = 1;

/// What this build adds on top of its dialect. Each side lists them in its
/// hello, and a peer ignores the ones it doesn't know.
///
/// `watched_prs`: the commands for repos and watching, and the
/// `watched_prs` topic.
///
/// `restart`: the `restart` command, which the GUI sends a daemon from
/// another build (ADR 0009).
///
/// `library`: the commands that list, save and delete Library Steps.
///
/// `runs`: Run history on `watched_prs` rows, the `run/<id>` topics, and
/// `unsubscribe`.
///
/// `run_control`: the `cancel_run` and `retry_step` commands, and the
/// `step_retried` Run event.
pub const FEATURES: &[&str] = &["watched_prs", "restart", "library", "runs", "run_control"];

/// The URL clients put in the WebSocket handshake. A Unix socket has no
/// host, so the host here is a placeholder the daemon ignores.
pub const LOCAL_URL: &str = "ws://localhost/";

/// The git SHA this binary was built from, plus a hash of the uncommitted
/// changes when the tree was dirty.
pub const BUILD_ID: &str = env!("SLOPWATCH_BUILD_ID");
