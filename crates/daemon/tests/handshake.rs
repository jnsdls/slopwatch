//! The hello exchange and requests, over the in-process transport.

use std::sync::Arc;
use std::time::Duration;

use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Peer, Watching};
use slopwatch_protocol::{
    Auth, ClientFrame, ClientHello, Command, DIALECT, ErrorCode, Refusal, RefusalReason, Reply,
    RequestId, Response, ResponseBody, ServerFrame, ServerHello,
};

fn daemon() -> Arc<Daemon> {
    let github = Arc::new(FakeGitHub::new("me"));
    let watching = Arc::new(Watching::new(Store::in_memory(), github).unwrap());
    Arc::new(Daemon::with_build_id("0123abcd+dirty.feed", watching))
}

async fn hello(client: &mut InProcessClient, hello: ClientHello) -> ServerFrame {
    client.send(&ClientFrame::Hello(hello)).await.unwrap();
    client
        .recv()
        .await
        .unwrap()
        .expect("daemon answered the hello")
}

async fn connected() -> InProcessClient {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();
    let answer = hello(&mut client, ClientHello::local()).await;
    assert!(matches!(answer, ServerFrame::Hello(_)), "got {answer:?}");
    client
}

fn refusal(frame: ServerFrame) -> Refusal {
    match frame {
        ServerFrame::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn response(frame: Option<ServerFrame>) -> Response {
    match frame {
        Some(ServerFrame::Response(response)) => response,
        other => panic!("expected a response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_matching_hello_is_answered_with_the_daemons_build_id() {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();

    let answer = hello(&mut client, ClientHello::local()).await;

    assert_eq!(
        answer,
        ServerFrame::Hello(ServerHello {
            dialect: DIALECT,
            features: vec!["watched_prs".into(), "restart".into()],
            build_id: "0123abcd+dirty.feed".into(),
        })
    );
}

#[tokio::test]
async fn a_dialect_mismatch_is_refused_with_a_restart_hint_and_closed() {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();
    let mut old = ClientHello::local();
    old.dialect = DIALECT + 1;

    let refused = refusal(hello(&mut client, old).await);

    assert_eq!(refused.reason, RefusalReason::DialectMismatch);
    assert!(
        refused.message.contains("Restart the daemon"),
        "{}",
        refused.message
    );
    assert_eq!(client.recv().await.unwrap(), None);
}

#[tokio::test]
async fn a_hello_from_another_dialect_gets_the_restart_hint_even_if_its_shape_changed() {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();

    client
        .send_text(format!(
            r#"{{"type":"hello","dialect":{},"credentials":{{"kind":"new"}}}}"#,
            DIALECT + 1
        ))
        .await
        .unwrap();
    let refused = refusal(client.recv().await.unwrap().unwrap());

    assert_eq!(refused.reason, RefusalReason::DialectMismatch);
    assert!(
        refused.message.contains("Restart the daemon"),
        "{}",
        refused.message
    );
}

#[tokio::test]
async fn a_peer_with_another_uid_is_refused() {
    let daemon = daemon();
    let stranger = Peer {
        uid: daemon.uid() + 1,
    };
    let mut client = InProcessClient::connect_as(daemon, stranger).await.unwrap();

    let refused = refusal(hello(&mut client, ClientHello::local()).await);

    assert_eq!(refused.reason, RefusalReason::PeerUidMismatch);
    assert_eq!(client.recv().await.unwrap(), None);
}

#[tokio::test]
async fn an_auth_scheme_other_than_local_is_refused() {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();
    let mut token = ClientHello::local();
    token.auth = Auth::Unsupported;

    let refused = refusal(hello(&mut client, token).await);

    assert_eq!(refused.reason, RefusalReason::UnsupportedAuth);
}

#[tokio::test]
async fn a_request_before_hello_is_refused() {
    let mut client = InProcessClient::connect(daemon()).await.unwrap();

    let refused = refusal(client.request(Command::Ping).await.unwrap().unwrap());

    assert_eq!(refused.reason, RefusalReason::ExpectedHello);
}

#[tokio::test]
async fn each_response_carries_its_requests_id() {
    let mut client = connected().await;

    let first = response(client.request(Command::Ping).await.unwrap());
    let second = response(client.request(Command::Ping).await.unwrap());

    assert_eq!(first.id, RequestId(1));
    assert_eq!(second.id, RequestId(2));
    assert_eq!(second.result, ResponseBody::Ok(Reply::Pong));
}

#[tokio::test]
async fn restart_is_answered_before_the_daemon_asks_to_exit() {
    let daemon = daemon();
    let mut client = InProcessClient::connect(Arc::clone(&daemon)).await.unwrap();
    hello(&mut client, ClientHello::local()).await;
    let requested = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move { daemon.restart_requested().await }
    });
    tokio::task::yield_now().await;
    assert!(!requested.is_finished(), "nobody asked for a restart yet");

    let answer = response(client.request(Command::Restart).await.unwrap());

    assert_eq!(answer.result, ResponseBody::Ok(Reply::Restarting));
    tokio::time::timeout(Duration::from_secs(5), requested)
        .await
        .expect("the daemon asks to exit once it answered")
        .unwrap();
}

#[tokio::test]
async fn an_unknown_command_gets_an_error_with_its_id() {
    let mut client = connected().await;

    client
        .send_text(
            r#"{"type":"request","id":41,"actor":{"kind":"developer","via":"test"},"command":{"name":"launch_rockets"}}"#,
        )
        .await
        .unwrap();
    let answer = response(client.recv().await.unwrap());

    assert_eq!(answer.id, RequestId(41));
    let ResponseBody::Error(error) = answer.result else {
        panic!("expected an error, got {:?}", answer.result);
    };
    assert_eq!(error.code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_request_without_an_actor_gets_an_error() {
    let mut client = connected().await;

    client
        .send_text(r#"{"type":"request","id":5,"command":{"name":"ping"}}"#)
        .await
        .unwrap();
    let answer = response(client.recv().await.unwrap());

    assert_eq!(answer.id, RequestId(5));
    assert!(
        matches!(answer.result, ResponseBody::Error(_)),
        "got {:?}",
        answer.result
    );
}
