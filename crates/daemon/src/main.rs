use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::Arc;

use slopwatch_daemon::auth::GhToken;
use slopwatch_daemon::github::api::Api;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, DataDir, Library, Watching};
use slopwatch_protocol::Flavor;

/// A log past this size starts over when the daemon starts.
const LOG_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    let path = Flavor::CURRENT.data_dir();
    let data_dir = match DataDir::lock(&path) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("slopwatchd: can't take {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    log_to_data_dir(&data_dir);

    let listener = match Listener::bind(&data_dir) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "slopwatchd: can't listen on {}: {error}",
                data_dir.socket_path().display()
            );
            return ExitCode::FAILURE;
        }
    };
    let db = data_dir.path().join("state.db");
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
    let steps = Library::default_dir(Flavor::CURRENT);
    let library = match Library::open(&steps) {
        Ok(library) => Arc::new(library),
        Err(error) => {
            eprintln!(
                "slopwatchd: can't open the Library at {}: {error}",
                steps.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let daemon = Arc::new(Daemon::new(Arc::clone(&watching), library));
    eprintln!(
        "slopwatchd: build {} listening on {}",
        daemon.build_id(),
        listener.path().display()
    );

    tokio::spawn(watching.run());
    // Crash-only (ADR 0009): no shutdown path. A signal kills the process,
    // and the next start replaces the stale socket. `restart` exits without
    // draining, and launchd starts whatever binary the bundle holds now.
    tokio::select! {
        () = listener.run(Arc::clone(&daemon)) => {}
        () = daemon.restart_requested() => {
            // No Steps run yet. Once they do, their process groups die here.
            eprintln!("slopwatchd: restarting on a client's request");
        }
    }
    ExitCode::SUCCESS
}

/// Under launchd, stderr goes nowhere, so the daemon appends it to
/// `daemon.log` in its data dir instead. Run from a terminal, it leaves
/// stderr alone.
fn log_to_data_dir(data_dir: &DataDir) {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(libc::STDERR_FILENO) } == 1 {
        return;
    }
    let path = data_dir.path().join("daemon.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT_BYTES) {
        let _ = fs::remove_file(&path);
    }
    let Ok(log) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    // SAFETY: both descriptors are open, and dup2 replaces stderr
    // atomically. `log` closing afterwards leaves the duplicate open.
    unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) };
}
