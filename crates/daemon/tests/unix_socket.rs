//! The real listener: socket file, lock and `getpeereid`.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use slopwatch_daemon::Daemon;
use slopwatch_daemon::transport::unix::Listener;
use slopwatch_protocol::{ClientFrame, ClientHello, LOCAL_URL, ServerFrame};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn a_client_on_the_socket_running_as_the_same_user_is_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.sock");
    let listener = Listener::bind(&path).unwrap();
    tokio::spawn(listener.run(Arc::new(Daemon::with_build_id("socket-build"))));

    let stream = UnixStream::connect(&path).await.unwrap();
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

#[tokio::test]
async fn a_second_daemon_cant_take_a_live_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.sock");
    let _first = Listener::bind(&path).unwrap();

    let second = Listener::bind(&path);

    assert_eq!(second.err().unwrap().kind(), std::io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn a_stale_socket_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.sock");
    drop(Listener::bind(&path).unwrap());
    assert!(
        path.exists(),
        "a crashed daemon leaves its socket file behind"
    );

    let listener = Listener::bind(&path);

    assert!(listener.is_ok(), "{:?}", listener.err());
}
