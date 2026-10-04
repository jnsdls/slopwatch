//! The GUI's link against a real daemon on a Unix socket.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slopwatch_client::agent::{Agent, AgentStatus};
use slopwatch_client::link::{self, ConnectError, Controls, LinkEvent, LinkState, Pace};
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, DataDir, Library, Watching};
use slopwatch_protocol::{
    BUILD_ID, ClientHello, Command, DIALECT, NotificationsUpdate, PrStatus, RefusalReason, Reply,
    RepoName, RequestId, Response, ResponseBody, TopicUpdate, WatchedPrsUpdate, socket_path,
};

const WAIT: Duration = Duration::from_secs(5);
const PACE: Pace = Pace {
    retry: Duration::from_millis(20),
    handoff_wait: Duration::from_millis(300),
};

/// A daemon serving a temp data dir. Dropping it stops the daemon the way a
/// crash would: every connection drops at once. A `restart` also stops it,
/// as the real binary exits.
struct RunningDaemon {
    runtime: Option<tokio::runtime::Runtime>,
}

impl RunningDaemon {
    fn start(dir: &Path, build_id: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        Self::start_with(dir, build_id, github, Store::in_memory())
    }

    fn start_with(dir: &Path, build_id: &str, github: Arc<FakeGitHub>, store: Store) -> Self {
        let deadline = Instant::now() + WAIT;
        // A daemon that just restarted may not have let go of the lock yet.
        let data_dir = loop {
            match DataDir::lock(dir) {
                Ok(data_dir) => break data_dir,
                Err(error) if Instant::now() > deadline => panic!("{error}"),
                Err(_) => std::thread::sleep(PACE.retry),
            }
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(async { Listener::bind(&data_dir) })
            .unwrap();
        let watching = Arc::new(Watching::new(store, github).unwrap());
        let library = Library::open(dir.join("steps")).unwrap();
        let daemon = Arc::new(Daemon::with_build_id(build_id, watching, Arc::new(library)));
        runtime.spawn(async move {
            tokio::select! {
                () = listener.run(Arc::clone(&daemon)) => {}
                () = daemon.restart_requested() => {}
            }
            drop(data_dir);
        });
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

/// Stands in for launchd: each re-register starts a daemon of the next
/// build in line on the data dir, or nothing for `None`, the way macOS
/// launches nothing on the first register after a rebuild without a Team
/// ID.
struct FakeAgent {
    dir: PathBuf,
    status: AgentStatus,
    starts: Mutex<Vec<Option<String>>>,
    reregistered: Mutex<Vec<Option<RunningDaemon>>>,
}

impl FakeAgent {
    fn new(dir: &Path, starts: &[Option<&str>]) -> Self {
        Self {
            dir: dir.to_owned(),
            status: AgentStatus::Enabled,
            starts: Mutex::new(starts.iter().rev().map(|b| b.map(str::to_owned)).collect()),
            reregistered: Mutex::new(Vec::new()),
        }
    }

    fn with_status(mut self, status: AgentStatus) -> Self {
        self.status = status;
        self
    }

    fn reregistrations(&self) -> usize {
        self.reregistered.lock().unwrap().len()
    }
}

impl Agent for FakeAgent {
    fn status(&self) -> AgentStatus {
        self.status
    }

    fn reregister(&self) -> Result<(), String> {
        let build = self.starts.lock().unwrap().pop().flatten();
        let daemon = build.map(|build| RunningDaemon::start(&self.dir, &build));
        self.reregistered.lock().unwrap().push(daemon);
        Ok(())
    }

    fn open_login_items(&self) {}
}

/// Runs the link on its own thread, as the GUI does, with `agent` and
/// the window's controls.
fn run_link_with(
    dir: &Path,
    agent: Option<Arc<FakeAgent>>,
    reregister: Receiver<()>,
) -> (Sender<(RequestId, Command)>, Receiver<LinkEvent>) {
    let (events, received) = mpsc::channel();
    let (commands, to_send) = mpsc::channel();
    let path = socket_path(dir);
    std::thread::spawn(move || {
        let agent = agent.as_deref().map(|agent| agent as &dyn Agent);
        let controls = Controls {
            commands: &to_send,
            reregister: &reregister,
        };
        link::run(&path, PACE, agent, controls, |event| {
            events.send(event).is_ok()
        });
    });
    (commands, received)
}

/// Runs the link outside a bundle: no agent, and nobody presses
/// Re-register.
fn run_link(dir: &Path) -> (Sender<(RequestId, Command)>, Receiver<LinkEvent>) {
    let (_, reregister) = mpsc::channel();
    run_link_with(dir, None, reregister)
}

/// The link's states, with `agent`, as Re-register presses on `button`
/// come in.
fn watch_with_button(
    dir: &Path,
    agent: Option<Arc<FakeAgent>>,
    button: Receiver<()>,
) -> Receiver<LinkState> {
    let (commands, events) = run_link_with(dir, agent, button);
    let (states, received) = mpsc::channel();
    std::thread::spawn(move || {
        let _commands = commands;
        while let Ok(event) = events.recv() {
            if let LinkEvent::State(state) = event
                && states.send(state).is_err()
            {
                return;
            }
        }
    });
    received
}

fn watch(dir: &Path, agent: Option<Arc<FakeAgent>>) -> Receiver<LinkState> {
    let (_, button) = mpsc::channel();
    watch_with_button(dir, agent, button)
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

fn next_reply(events: &Receiver<LinkEvent>) -> Response {
    loop {
        if let LinkEvent::Response(response) = events.recv_timeout(WAIT).unwrap()
            && response.result != ResponseBody::Ok(Reply::Done)
        {
            return response;
        }
    }
}

#[test]
fn connecting_to_a_running_daemon_reports_its_build_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path());
    let _daemon = RunningDaemon::start(dir.path(), "abc123+dirty.feed");

    let session = link::connect(&path, &ClientHello::local()).unwrap();

    assert_eq!(session.daemon().build_id, "abc123+dirty.feed");
}

#[test]
fn connecting_with_nothing_on_the_socket_means_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path());

    let error = link::connect(&path, &ClientHello::local()).err().unwrap();

    assert!(matches!(error, ConnectError::NotRunning(_)), "{error:?}");
}

#[test]
fn a_dialect_mismatch_reports_the_daemons_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path());
    let _daemon = RunningDaemon::start(dir.path(), "abc123");
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
    let dir = tempfile::tempdir().unwrap();
    let (_commands, events) = run_link(dir.path());

    assert_eq!(next_state(&events), LinkState::NotRunning { agent: None });

    let daemon = RunningDaemon::start(dir.path(), BUILD_ID);
    assert_eq!(
        next_state(&events),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );

    drop(daemon);
    assert_eq!(next_state(&events), LinkState::NotRunning { agent: None });

    let _daemon = RunningDaemon::start(dir.path(), BUILD_ID);
    assert_eq!(
        next_state(&events),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );
}

#[test]
fn the_link_follows_the_notifications_so_the_daemon_knows_a_gui_listens() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = RunningDaemon::start(dir.path(), BUILD_ID);
    let (_commands, events) = run_link(dir.path());

    let update = loop {
        if let LinkEvent::Topic(TopicUpdate::Notifications { update, .. }) =
            events.recv_timeout(WAIT).unwrap()
        {
            break update;
        }
    };

    assert_eq!(update, NotificationsUpdate::Snapshot(Vec::new()));
}

#[test]
fn the_link_subscribes_sends_commands_and_resubscribes_after_a_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let repo = RepoName::new("jnsdls", "app");
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo);
    github.open_pr(&repo, 1, "me", "Add the thing");
    let db = dir.path().join("state.db");
    let store = Store::open(&db).unwrap();
    let daemon = RunningDaemon::start_with(dir.path(), BUILD_ID, Arc::clone(&github), store);
    let (commands, events) = run_link(dir.path());

    let WatchedPrsUpdate::Snapshot(empty) = next_update(&events) else {
        panic!("a connection starts with a snapshot");
    };
    assert!(empty.repos.is_empty());

    commands
        .send((RequestId(1), Command::ListAvailableRepos))
        .unwrap();
    assert_eq!(
        next_reply(&events),
        Response {
            id: RequestId(1),
            result: ResponseBody::Ok(Reply::AvailableRepos {
                repos: vec![repo.clone()]
            }),
        },
        "the answer carries the id the window gave"
    );

    commands
        .send((RequestId(2), Command::AddRepo { repo: repo.clone() }))
        .unwrap();
    commands
        .send((
            RequestId(3),
            Command::Watch {
                repo: repo.clone(),
                number: 1,
            },
        ))
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
    let _daemon = RunningDaemon::start_with(dir.path(), BUILD_ID, github, store);
    let WatchedPrsUpdate::Snapshot(fresh) = next_update(&events) else {
        panic!("a reconnect starts with a fresh snapshot");
    };
    assert_eq!(fresh.repos, std::slice::from_ref(&repo));
    assert_eq!(
        fresh.pr(&repo, 1).map(|pr| pr.status),
        Some(PrStatus::Waiting)
    );
}

#[test]
fn a_daemon_from_another_build_is_restarted_and_re_registered() {
    let dir = tempfile::tempdir().unwrap();
    let _old = RunningDaemon::start(dir.path(), "old-build");
    let agent = Arc::new(FakeAgent::new(dir.path(), &[Some(BUILD_ID)]));

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );
    assert_eq!(agent.reregistrations(), 1);
}

#[test]
fn a_register_that_launches_nothing_is_retried_once() {
    let dir = tempfile::tempdir().unwrap();
    let _old = RunningDaemon::start(dir.path(), "old-build");
    let agent = Arc::new(FakeAgent::new(dir.path(), &[None, Some(BUILD_ID)]));

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );
    assert_eq!(agent.reregistrations(), 2);
}

#[test]
fn a_handoff_that_never_brings_up_a_daemon_ends_in_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let _old = RunningDaemon::start(dir.path(), "old-build");
    let agent = Arc::new(FakeAgent::new(dir.path(), &[None, None]));

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::NotRunning {
            agent: Some(AgentStatus::Enabled)
        }
    );
    assert_eq!(agent.reregistrations(), 2);
}

#[test]
fn a_handoff_that_keeps_bringing_back_the_old_build_stops() {
    let dir = tempfile::tempdir().unwrap();
    let _old = RunningDaemon::start(dir.path(), "old-build");
    let agent = Arc::new(FakeAgent::new(
        dir.path(),
        &[Some("old-build"), Some("old-build"), Some("old-build")],
    ));

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Mismatched {
            daemon_build_id: "old-build".into()
        }
    );
    assert_eq!(agent.reregistrations(), 2);
}

#[test]
fn without_an_agent_another_build_is_only_reported() {
    let dir = tempfile::tempdir().unwrap();
    let _old = RunningDaemon::start(dir.path(), "old-build");

    let received = watch(dir.path(), None);

    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Mismatched {
            daemon_build_id: "old-build".into()
        }
    );
}

#[test]
fn a_daemon_that_isnt_running_at_launch_is_registered() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(FakeAgent::new(dir.path(), &[Some(BUILD_ID)]));

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );
    assert_eq!(agent.reregistrations(), 1);
}

#[test]
fn a_launch_registration_that_brings_up_nothing_ends_in_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(FakeAgent::new(dir.path(), &[None, None]));

    let received = watch(dir.path(), Some(agent));

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::NotRunning {
            agent: Some(AgentStatus::Enabled)
        }
    );
}

#[test]
fn an_agent_turned_off_in_login_items_stays_off() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(
        FakeAgent::new(dir.path(), &[Some(BUILD_ID)]).with_status(AgentStatus::RequiresApproval),
    );

    let received = watch(dir.path(), Some(Arc::clone(&agent)));

    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::NotRunning {
            agent: Some(AgentStatus::RequiresApproval)
        }
    );
    assert_eq!(agent.reregistrations(), 0);
}

#[test]
fn re_register_brings_back_a_daemon_that_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(FakeAgent::new(
        dir.path(),
        &[None, None, None, Some(BUILD_ID)],
    ));
    let (reregister, requests) = mpsc::channel();
    let received = watch_with_button(dir.path(), Some(Arc::clone(&agent)), requests);
    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::NotRunning {
            agent: Some(AgentStatus::Enabled)
        }
    );

    reregister.send(()).unwrap();

    assert_eq!(received.recv_timeout(WAIT).unwrap(), LinkState::HandingOff);
    assert_eq!(
        received.recv_timeout(WAIT).unwrap(),
        LinkState::Connected {
            daemon_build_id: BUILD_ID.into()
        }
    );
    assert_eq!(agent.reregistrations(), 4);
}
