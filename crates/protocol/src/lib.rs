//! The wire contract between slopwatch's daemon and its clients.
//!
//! Clients speak WebSocket with one JSON object per text frame, over the
//! daemon's Unix socket in v1 (ADR 0010). The first frame each side sends is
//! a hello. After that the client sends requests and the daemon answers each
//! one with a response carrying the same id. A client that subscribes to a
//! topic also gets topic updates: a snapshot, then ordered deltas.

mod budgets;
mod flavor;
mod frames;
mod inbox;
pub mod logs;
mod notifications;
pub mod pipeline;
mod plugins;
pub mod runs;
mod secrets;
pub mod step;
mod topics;

pub use budgets::{BudgetHit, BudgetKind, Cents, DAILY_BUDGET_DEFAULT, DaemonSettings};
pub use flavor::{DATA_DIR_ENV, Flavor, socket_path};
pub use frames::{
    Actor, Auth, ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, LibraryStep, Refusal,
    RefusalReason, Reply, Request, RequestId, Response, ResponseBody, ServerFrame, ServerHello,
};
pub use inbox::{
    Answer, Cause, Closed, Closing, EntryId, Inbox, InboxDelta, InboxEntry, InboxUpdate, PrRef,
    Scope,
};
pub use logs::{
    LogFilter, LogKey, LogLevel, LogPage, LogRecord, LogSource, MAX_PAGE, StepLogPage,
    StorageWarning, Truncation,
};
pub use notifications::{
    About, Notification, NotificationId, NotificationsDelta, NotificationsUpdate,
};
pub use plugins::{Grant, PluginListing, PluginSettings, workspace_name};
pub use runs::{
    Cost, EffectView, GateTerm, RunEvent, RunId, RunSummary, RunView, StepInfo, StepStatus,
    StepView, Waiver,
};
pub use secrets::{SecretInfo, SecretValue, is_secret_name};
pub use topics::{
    PollState, PrStatus, PullRequest, RepoName, StackParent, StackPlace, Topic, TopicUpdate,
    WatchedPrs, WatchedPrsDelta, WatchedPrsUpdate,
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
///
/// `step_logs`: the `log/<run>/<step>/<attempt>` topics, `read_step_log`,
/// timestamps on Run events, and the storage warning on `watched_prs`.
///
/// `waivers`: the `waive_step` and `override_gate` commands, the
/// `step_waived` Run event, and the `waived` mark on an ended Run.
///
/// `inbox`: the `inbox` topic, the `dismiss_entry` command, and the
/// `inbox` Run event.
///
/// `notifications`: the `notifications` topic and the
/// `ack_notifications` command (ADR 0013).
///
/// `secrets`: the `list_secrets`, `set_secret` and `delete_secret`
/// commands, and the `missing_secret` Inbox cause.
///
/// `human_steps`: the `answer_step` command and Human Step entries in the
/// Inbox.
///
/// `pipeline_editor`: the `pipeline/<owner>/<name>` topics, and the
/// `edit_pipeline`, `move_pipeline_node` and `tidy_pipeline` commands.
///
/// `plugins`: the `list_plugins`, `approve_plugin` and
/// `set_plugin_settings` commands, and the `unapproved_plugin` Inbox
/// cause (ADR 0012).
///
/// `pipeline_publish`: the `publish_pipeline`, `merge_pipeline` and
/// `discard_pipeline_draft` commands, and `published` and `conflicts` on a
/// draft.
///
/// `onboarding`: the `apply_starter` command, and `missing_secrets` and
/// `missing_plugin` on a draft's Steps.
///
/// `budgets`: the `raise_budget`, `run_anyway_once`, `get_settings` and
/// `set_settings` commands, the `daily_budget` Inbox cause, and `budget`
/// on an entry a spent Budget raised.
pub const FEATURES: &[&str] = &[
    "watched_prs",
    "restart",
    "library",
    "runs",
    "run_control",
    "step_logs",
    "waivers",
    "inbox",
    "notifications",
    "secrets",
    "human_steps",
    "pipeline_editor",
    "plugins",
    "stacks",
    "pipeline_publish",
    "onboarding",
    "budgets",
];

/// The URL clients put in the WebSocket handshake. A Unix socket has no
/// host, so the host here is a placeholder the daemon ignores.
pub const LOCAL_URL: &str = "ws://localhost/";

/// The git SHA this binary was built from, plus a hash of the uncommitted
/// changes when the tree was dirty.
pub const BUILD_ID: &str = env!("SLOPWATCH_BUILD_ID");
