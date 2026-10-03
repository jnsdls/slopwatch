use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use slopwatch_protocol::{
    ClientFrame, Command, ErrorBody, ErrorCode, Reply, Request, RequestId, Response, ResponseBody,
    ServerFrame, Topic, TopicUpdate, WatchedPrsUpdate,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

use crate::{Daemon, Peer, Subscription, WatchError};

/// How long a client gets to send its hello before the daemon hangs up.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

impl Daemon {
    /// Serves one client: the WebSocket handshake, the hello exchange, then
    /// requests and topic updates until the client goes away.
    pub async fn serve<S>(&self, stream: S, peer: Peer) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ws = tokio_tungstenite::accept_async(stream).await?;

        let hello = match tokio::time::timeout(HELLO_TIMEOUT, next_text(&mut ws)).await {
            Ok(Ok(Some(text))) => text,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => return Err(error),
            Err(_) => return ws.close(None).await,
        };
        match self.admit(&hello, peer) {
            Ok(welcome) => send(&mut ws, &ServerFrame::Hello(welcome)).await?,
            Err(refusal) => {
                send(&mut ws, &ServerFrame::Refused(refusal)).await?;
                return ws.close(None).await;
            }
        }

        let mut deltas: Option<broadcast::Receiver<_>> = None;
        loop {
            tokio::select! {
                text = next_text(&mut ws) => {
                    let Some(text) = text? else { return Ok(()) };
                    let Some((id, command)) = self.parse(&text, &mut ws).await? else {
                        continue;
                    };
                    let subscribe = matches!(command, Command::Subscribe { .. });
                    let result = self.execute(command).await;
                    if subscribe && matches!(result, ResponseBody::Ok(_)) {
                        let subscription = self.watching.subscribe();
                        deltas = Some(subscription.deltas);
                        send(&mut ws, &snapshot(subscription.seq, subscription.snapshot)).await?;
                    }
                    // A command's deltas reach the client before its
                    // response, so a client that waits for the response
                    // sees what the command changed.
                    if let Some(receiver) = &mut deltas {
                        while let Some(frame) = self.forward(receiver.try_recv()) {
                            send(&mut ws, &frame).await?;
                        }
                    }
                    send(&mut ws, &ServerFrame::Response(Response { id, result })).await?;
                }
                delta = recv(&mut deltas) => {
                    if let Some(frame) = self.forward(delta) {
                        send(&mut ws, &frame).await?;
                    }
                }
            }
        }
    }

    /// Reads a request. A malformed one with an id gets an error right
    /// away, and anything else without an id is dropped.
    async fn parse<S>(
        &self,
        text: &str,
        ws: &mut WebSocketStream<S>,
    ) -> Result<Option<(RequestId, Command)>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match serde_json::from_str::<ClientFrame>(text) {
            Ok(ClientFrame::Request(Request {
                id,
                actor: _,
                command,
            })) => Ok(Some((id, command))),
            // A connection says hello once. A second one has no id to answer.
            Ok(ClientFrame::Hello(_)) => Ok(None),
            Err(error) => {
                if let Some(id) = request_id(text) {
                    let result = ResponseBody::Error(ErrorBody {
                        code: ErrorCode::BadRequest,
                        message: error.to_string(),
                    });
                    send(ws, &ServerFrame::Response(Response { id, result })).await?;
                }
                Ok(None)
            }
        }
    }

    async fn execute(&self, command: Command) -> ResponseBody {
        let watching = &self.watching;
        let result = match command {
            Command::Ping => Ok(Reply::Pong),
            Command::ListAvailableRepos => watching
                .available_repos()
                .await
                .map(|repos| Reply::AvailableRepos { repos }),
            Command::AddRepo { repo } => watching.add_repo(repo).await.map(|()| Reply::Done),
            Command::Watch { repo, number } => watching
                .set_watched(repo, number, true)
                .await
                .map(|()| Reply::Done),
            Command::Unwatch { repo, number } => watching
                .set_watched(repo, number, false)
                .await
                .map(|()| Reply::Done),
            Command::Refresh => watching.poll().await.map(|()| Reply::Done),
            Command::Subscribe {
                topic: Topic::WatchedPrs,
            } => Ok(Reply::Done),
        };
        match result {
            Ok(reply) => ResponseBody::Ok(reply),
            Err(error) => ResponseBody::Error(error_body(error)),
        }
    }

    /// The frame for a delta, or a fresh snapshot for a subscriber that fell
    /// too far behind to catch up.
    fn forward(
        &self,
        delta: Result<(u64, slopwatch_protocol::WatchedPrsDelta), impl Into<Missed>>,
    ) -> Option<ServerFrame> {
        match delta.map_err(Into::into) {
            Ok((seq, delta)) => Some(ServerFrame::Topic(TopicUpdate::WatchedPrs {
                seq,
                update: WatchedPrsUpdate::Delta(delta),
            })),
            Err(Missed::Nothing) => None,
            Err(Missed::Lagged) => {
                let Subscription { seq, snapshot, .. } = self.watching.subscribe();
                Some(self::snapshot(seq, snapshot))
            }
        }
    }
}

/// Why a subscriber got no delta.
enum Missed {
    Nothing,
    /// It fell behind and lost deltas. It resyncs from a snapshot, and the
    /// deltas still queued after that are older than the snapshot, so they
    /// arrive with lower sequence numbers and the client drops them.
    Lagged,
}

impl From<broadcast::error::TryRecvError> for Missed {
    fn from(error: broadcast::error::TryRecvError) -> Self {
        match error {
            broadcast::error::TryRecvError::Lagged(_) => Missed::Lagged,
            _ => Missed::Nothing,
        }
    }
}

impl From<broadcast::error::RecvError> for Missed {
    fn from(error: broadcast::error::RecvError) -> Self {
        match error {
            broadcast::error::RecvError::Lagged(_) => Missed::Lagged,
            broadcast::error::RecvError::Closed => Missed::Nothing,
        }
    }
}

fn snapshot(seq: u64, snapshot: slopwatch_protocol::WatchedPrs) -> ServerFrame {
    ServerFrame::Topic(TopicUpdate::WatchedPrs {
        seq,
        update: WatchedPrsUpdate::Snapshot(snapshot),
    })
}

/// The next delta for a subscribed connection. Never resolves for one that
/// hasn't subscribed.
async fn recv<T: Clone>(
    receiver: &mut Option<broadcast::Receiver<T>>,
) -> Result<T, broadcast::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn error_body(error: WatchError) -> ErrorBody {
    let (code, message) = match error {
        WatchError::NotFound(what) => (ErrorCode::NotFound, format!("Not found: {what}")),
        WatchError::GitHub(error) => (ErrorCode::GitHub, error.to_string()),
        WatchError::Store(error) => (ErrorCode::Internal, format!("Database error: {error}")),
    };
    ErrorBody { code, message }
}

fn request_id(text: &str) -> Option<RequestId> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "request" {
        return None;
    }
    value.get("id")?.as_u64().map(RequestId)
}

/// The next text frame, or `None` once the client closed the connection.
/// tungstenite answers pings itself, and binary frames carry nothing in this
/// protocol.
pub(crate) async fn next_text<S>(ws: &mut WebSocketStream<S>) -> Result<Option<String>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Text(text)) => return Ok(Some(text.to_string())),
            Ok(Message::Close(_)) => return Ok(None),
            Ok(_) => continue,
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

async fn send<S>(ws: &mut WebSocketStream<S>, frame: &ServerFrame) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let text = serde_json::to_string(frame).expect("server frames always serialize");
    ws.send(Message::text(text)).await
}
