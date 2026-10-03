use std::process::ExitCode;
use std::sync::Arc;

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, Watching};

#[tokio::main]
async fn main() -> ExitCode {
    let path = slopwatch_protocol::local_socket_path();
    let listener = match Listener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("slopwatchd: can't listen on {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    // The database sits next to the socket, so overriding the socket path
    // also gives a second daemon its own state.
    let db = path.with_file_name("state.db");
    let store = match Store::open(&db) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("slopwatchd: can't open {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let github = Arc::new(Api::new(Arc::new(GhToken::default())));
    let watching = match Watching::new(store, github) {
        Ok(watching) => Arc::new(watching),
        Err(error) => {
            eprintln!("slopwatchd: can't load {}: {error}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let daemon = Arc::new(Daemon::new(Arc::clone(&watching)));
    eprintln!(
        "slopwatchd: build {} listening on {}",
        daemon.build_id(),
        listener.path().display()
    );

    tokio::spawn(watching.run());
    // Crash-only (ADR 0009): no shutdown path. A signal kills the process,
    // and the next start replaces the stale socket.
    listener.run(daemon).await;
    ExitCode::SUCCESS
}
