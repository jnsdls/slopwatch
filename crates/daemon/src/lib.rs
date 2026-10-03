//! The slopwatch daemon.
//!
//! [`Daemon::serve`] serves one client connection over any byte stream, and
//! a transport runs one per connected client. The [`transport`] module feeds
//! it streams: [`transport::unix`] from the
//! Unix socket, and [`transport::in_process`] from an in-memory duplex that
//! carries the same WebSocket frames, for tests.
//!
//! [`Watching`] keeps added repos and their PRs in step with GitHub, which
//! it reaches only through the [`github::GitHub`] trait. [`Runs`] runs each
//! Watched PR through its Pipeline, following what each poll saw.
//!
//! [`Library`] holds the developer's Library Steps, which clients list and
//! edit, and which a Pipeline's resolver reads live. [`drafts::Drafts`]
//! keeps each repo's draft Pipeline for the editor.

pub mod approvals;
pub mod auth;
pub mod clones;
mod connection;
mod data_dir;
pub mod drafts;
pub mod github;
pub mod inbox;
mod library;
pub mod notifications;
mod pace;
pub mod plugins;
pub mod runs;
pub mod secrets;
pub mod shell_env;
pub mod store;
pub mod transport;
mod watching;

use std::sync::Arc;

pub use data_dir::DataDir;
use drafts::Drafts;

use slopwatch_protocol::{
    Auth, BUILD_ID, ClientFrame, ClientHello, DIALECT, FEATURES, Refusal, RefusalReason,
    ServerHello,
};

pub use library::{CONFIG_DIR_ENV, Library, LibraryError, config_dir};
pub use runs::{Retention, RunError, Runs, RunsConfig, STEP_CAP};
pub use watching::{RunInfo, Subscription, WatchError, Watching};

/// Who is on the other end of a connection, as the transport reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
}

pub struct Daemon {
    build_id: String,
    uid: u32,
    watching: Arc<Watching>,
    /// `None` for a daemon that only watches PRs, as some tests build.
    runs: Option<Arc<Runs>>,
    restart: tokio::sync::watch::Sender<bool>,
    library: Arc<Library>,
    /// `None` for a daemon without the Pipeline editor, as some tests build.
    drafts: Option<Arc<Drafts>>,
}

impl Daemon {
    /// A daemon stamped with this binary's build id, serving peers that run
    /// as the current user.
    pub fn new(watching: Arc<Watching>, library: Arc<Library>) -> Self {
        Self::with_build_id(BUILD_ID, watching, library)
    }

    pub fn with_build_id(
        build_id: impl Into<String>,
        watching: Arc<Watching>,
        library: Arc<Library>,
    ) -> Self {
        Self {
            build_id: build_id.into(),
            uid: current_uid(),
            watching,
            runs: None,
            restart: tokio::sync::watch::Sender::new(false),
            library,
            drafts: None,
        }
    }

    /// Gives the daemon draft Pipelines to keep and serve.
    pub fn with_drafts(mut self, drafts: Arc<Drafts>) -> Self {
        self.drafts = Some(drafts);
        self
    }

    /// Gives the daemon Runs to drive and serve.
    pub fn with_runs(mut self, runs: Arc<Runs>) -> Self {
        self.runs = Some(runs);
        self
    }

    /// Polls GitHub, then brings Runs in line with what the poll saw.
    pub async fn poll(&self) -> Result<(), WatchError> {
        let polled = self.watching.poll().await;
        self.sync_runs().await;
        polled
    }

    async fn sync_runs(&self) {
        if let Some(runs) = &self.runs {
            runs.sync().await;
        }
    }

    /// Polls forever, at the pace the rate budget allows: faster while a
    /// Run is going.
    pub async fn poll_forever(self: Arc<Self>) {
        loop {
            if let Err(error) = self.poll().await {
                eprintln!("slopwatchd: poll failed: {error:?}");
            }
            let live = self.runs.as_ref().is_some_and(|runs| runs.live());
            tokio::time::sleep_until(self.watching.next_poll(live).into()).await;
        }
    }

    /// Kills every running Step's process group, for an exit with no drain
    /// (ADR 0009).
    pub async fn kill_steps(&self) {
        if let Some(runs) = &self.runs {
            runs.kill_steps().await;
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

    /// Resolves once a client's `restart` has been answered. The process
    /// then kills its Step process groups and exits, with no drain
    /// (ADR 0009).
    pub async fn restart_requested(&self) {
        let mut requested = self.restart.subscribe();
        // The sender lives in `self`, so the channel can't close under us.
        let _ = requested.wait_for(|requested| *requested).await;
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
                features: FEATURES.iter().map(|&feature| feature.to_owned()).collect(),
                build_id: self.build_id.clone(),
            }),
            Auth::Unsupported => Err(Refusal {
                reason: RefusalReason::UnsupportedAuth,
                message: "This daemon accepts only local auth.".to_owned(),
            }),
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments, always succeeds and touches no memory.
    unsafe { libc::getuid() }
}
