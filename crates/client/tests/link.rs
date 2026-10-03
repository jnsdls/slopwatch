//! The GUI's link against a real daemon on a Unix socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use slopwatch_client::link::{self, ConnectError, Status};
use slopwatch_daemon::Daemon;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_protocol::{ClientHello, DIALECT, RefusalReason};

const WAIT: Duration = Duration::from_secs(5);

/// A daemon listening on a socket in a temp dir. Dropping it stops the
/// daemon the way a crash would: every connection drops at once.
struct RunningDaemon {
    runtime: Option<tokio::runtime::Runtime>,
}

impl RunningDaemon {
    fn start(path: &Path, build_id: &str) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime.block_on(async { Listener::bind(path) }).unwrap();
        runtime.spawn(listener.run(Arc::new(Daemon::with_build_id(build_id))));
        Self {
            runtime: Some(runtime),
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.runtime.take().unwrap().shutdown_background();
    }
}

fn socket() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.sock");
    (dir, path)
}

#[test]
fn connecting_to_a_running_daemon_reports_its_build_id() {
    let (_dir, path) = socket();
    let _daemon = RunningDaemon::start(&path, "abc123+dirty.feed");

    let session = link::connect(&path, &ClientHello::local()).unwrap();

    assert_eq!(session.daemon().build_id, "abc123+dirty.feed");
}

#[test]
fn connecting_with_nothing_on_the_socket_means_not_running() {
    let (_dir, path) = socket();

    let error = link::connect(&path, &ClientHello::local()).err().unwrap();

    assert!(matches!(error, ConnectError::NotRunning(_)), "{error:?}");
}

#[test]
fn a_dialect_mismatch_reports_the_daemons_refusal() {
    let (_dir, path) = socket();
    let _daemon = RunningDaemon::start(&path, "abc123");
    let mut old = ClientHello::local();
    old.dialect = DIALECT + 1;

    let error = link::connect(&path, &old).err().unwrap();

    let ConnectError::Refused(refusal) = error else {
        panic!("expected a refusal, got {error:?}");
    };
    assert_eq!(refusal.reason, RefusalReason::DialectMismatch);
}

#[test]
fn watching_follows_the_daemon_going_away_and_coming_back() {
    let (_dir, path) = socket();
    let (statuses, received) = mpsc::channel();
    let watched = path.clone();
    std::thread::spawn(move || {
        link::watch(&watched, Duration::from_millis(20), |status| {
            statuses.send(status).is_ok()
        });
    });

    assert_eq!(received.recv_timeout(WAIT).unwrap(), Status::NotRunning);

    let daemon = RunningDaemon::start(&path, "first");
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        Status::Connected {
            daemon_build_id: "first".into()
        }
    );

    drop(daemon);
    assert_eq!(received.recv_timeout(WAIT).unwrap(), Status::NotRunning);

    let _daemon = RunningDaemon::start(&path, "second");
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        Status::Connected {
            daemon_build_id: "second".into()
        }
    );
}
