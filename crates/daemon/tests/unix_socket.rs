//! The real listener: data dir lock, socket file and `getpeereid`.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_daemon::{Daemon, DataDir, Library, Watching};
use slopwatch_protocol::{ClientFrame, ClientHello, LOCAL_URL, ServerFrame};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn a_client_on_the_socket_running_as_the_same_user_is_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = DataDir::lock(dir.path()).unwrap();
    let listener = Listener::bind(&data_dir).unwrap();
    let github = Arc::new(FakeGitHub::new("me"));
    let watching = Arc::new(Watching::new(Store::in_memory(), github).unwrap());
    let library = Arc::new(Library::open(dir.path().join("steps")).unwrap());
    tokio::spawn(listener.run(Arc::new(Daemon::with_build_id(
        "socket-build",
        watching,
        library,
    ))));

    let stream = UnixStream::connect(data_dir.socket_path()).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(LOCAL_URL, stream)
        .await
        .unwrap();
    let hello = ClientFrame::Hello(ClientHello::local());
    ws.send(Message::text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();
    let Some(Ok(Message::Text(answer))) = ws.next().await else {
        panic!("no answer to hello");
    };

    let ServerFrame::Hello(welcome) = serde_json::from_str(&answer).unwrap() else {
        panic!("expected a hello, got {answer}");
    };
    assert_eq!(welcome.build_id, "socket-build");
}

#[test]
fn a_second_daemon_cant_lock_a_held_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let _first = DataDir::lock(dir.path()).unwrap();

    let second = DataDir::lock(dir.path());

    assert_eq!(
        second.err().unwrap().kind(),
        std::io::ErrorKind::ResourceBusy
    );
}

#[test]
fn a_data_dir_unlocks_when_its_daemon_goes() {
    let dir = tempfile::tempdir().unwrap();
    drop(DataDir::lock(dir.path()).unwrap());

    assert!(DataDir::lock(dir.path()).is_ok());
}

#[test]
fn locking_creates_a_missing_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Application Support/slopwatch-dev");

    let data_dir = DataDir::lock(&path).unwrap();

    assert!(data_dir.path().is_dir());
}

#[tokio::test]
async fn a_stale_socket_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = DataDir::lock(dir.path()).unwrap();
    drop(Listener::bind(&data_dir).unwrap());
    assert!(
        data_dir.socket_path().exists(),
        "a crashed daemon leaves its socket file behind"
    );

    let listener = Listener::bind(&data_dir);

    assert!(listener.is_ok(), "{:?}", listener.err());
}
