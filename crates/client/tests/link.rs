//! The GUI's link against a real daemon on a Unix socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use slopwatch_client::link::{self, ConnectError, LinkEvent, LinkState};
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, Watching};
use slopwatch_protocol::{
    ClientHello, Command, DIALECT, PrStatus, RefusalReason, Reply, RepoName, ResponseBody,
    TopicUpdate, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(5);

/// A daemon listening on a socket in a temp dir. Dropping it stops the
/// daemon the way a crash would: every connection drops at once.
struct RunningDaemon {
    runtime: Option<tokio::runtime::Runtime>,
}

impl RunningDaemon {
    fn start(path: &Path, build_id: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        Self::start_with(path, build_id, github, Store::in_memory())
    }

    fn start_with(path: &Path, build_id: &str, github: Arc<FakeGitHub>, store: Store) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime.block_on(async { Listener::bind(path) }).unwrap();
        let watching = Arc::new(Watching::new(store, github).unwrap());
        runtime.spawn(listener.run(Arc::new(Daemon::with_build_id(build_id, watching))));
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

/// Runs the link on its own thread, as the GUI does.
fn run_link(path: &Path) -> (Sender<Command>, Receiver<LinkEvent>) {
    let (events, received) = mpsc::channel();
    let (commands, to_send) = mpsc::channel();
    let path = path.to_owned();
    std::thread::spawn(move || {
        link::run(&path, Duration::from_millis(20), &to_send, |event| {
            events.send(event).is_ok()
        });
    });
    (commands, received)
}

fn next_state(events: &Receiver<LinkEvent>) -> LinkState {
    loop {
        if let LinkEvent::State(state) = events.recv_timeout(WAIT).unwrap() {
            return state;
        }
    }
}

fn next_update(events: &Receiver<LinkEvent>) -> WatchedPrsUpdate {
    loop {
        if let LinkEvent::Topic(TopicUpdate::WatchedPrs { update, .. }) =
            events.recv_timeout(WAIT).unwrap()
        {
            return update;
        }
    }
}

fn next_reply(events: &Receiver<LinkEvent>) -> ResponseBody {
    loop {
        if let LinkEvent::Response(response) = events.recv_timeout(WAIT).unwrap()
            && response.result != ResponseBody::Ok(Reply::Done)
        {
            return response.result;
        }
    }
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
fn the_link_follows_the_daemon_going_away_and_coming_back() {
    let (_dir, path) = socket();
    let (_commands, events) = run_link(&path);

    assert_eq!(next_state(&events), LinkState::NotRunning);

    let daemon = RunningDaemon::start(&path, "first");
    assert_eq!(
        next_state(&events),
        LinkState::Connected {
            daemon_build_id: "first".into()
        }
    );

    drop(daemon);
    assert_eq!(next_state(&events), LinkState::NotRunning);

    let _daemon = RunningDaemon::start(&path, "second");
    assert_eq!(
        next_state(&events),
        LinkState::Connected {
            daemon_build_id: "second".into()
        }
    );
}

#[test]
fn the_link_subscribes_sends_commands_and_resubscribes_after_a_reconnect() {
    let (_dir, path) = socket();
    let repo = RepoName::new("jnsdls", "app");
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo);
    github.open_pr(&repo, 1, "me", "Add the thing");
    let db = path.with_file_name("state.db");
    let store = Store::open(&db).unwrap();
    let daemon = RunningDaemon::start_with(&path, "first", Arc::clone(&github), store);
    let (commands, events) = run_link(&path);

    let WatchedPrsUpdate::Snapshot(empty) = next_update(&events) else {
        panic!("a connection starts with a snapshot");
    };
    assert!(empty.repos.is_empty());

    commands.send(Command::ListAvailableRepos).unwrap();
    assert_eq!(
        next_reply(&events),
        ResponseBody::Ok(Reply::AvailableRepos {
            repos: vec![repo.clone()]
        })
    );

    commands
        .send(Command::AddRepo { repo: repo.clone() })
        .unwrap();
    commands
        .send(Command::Watch {
            repo: repo.clone(),
            number: 1,
        })
        .unwrap();
    let mut statuses = Vec::new();
    while statuses.last() != Some(&PrStatus::Waiting) {
        if let WatchedPrsUpdate::Delta(slopwatch_protocol::WatchedPrsDelta::PrChanged { pr }) =
            next_update(&events)
        {
            statuses.push(pr.status);
        }
    }
    assert_eq!(statuses, [PrStatus::NotWatched, PrStatus::Waiting]);
    assert!(github.is_labeled(&repo, 1));

    drop(daemon);
    let store = Store::open(&db).unwrap();
    let _daemon = RunningDaemon::start_with(&path, "second", github, store);
    let WatchedPrsUpdate::Snapshot(fresh) = next_update(&events) else {
        panic!("a reconnect starts with a fresh snapshot");
    };
    assert_eq!(fresh.repos, std::slice::from_ref(&repo));
    assert_eq!(
        fresh.pr(&repo, 1).map(|pr| pr.status),
        Some(PrStatus::Waiting)
    );
}
