use std::process::ExitCode;
use std::sync::Arc;

use slopwatch_daemon::Daemon;
use slopwatch_daemon::transport::unix::Listener;

#[tokio::main]
async fn main() -> ExitCode {
    let daemon = Arc::new(Daemon::new());
    let path = slopwatch_protocol::local_socket_path();
    let listener = match Listener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("slopwatchd: can't listen on {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "slopwatchd: build {} listening on {}",
        daemon.build_id(),
        listener.path().display()
    );

    // Crash-only (ADR 0009): no shutdown path. A signal kills the process,
    // and the next start replaces the stale socket.
    match listener.run(daemon).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("slopwatchd: accept failed: {error}");
            ExitCode::FAILURE
        }
    }
}
