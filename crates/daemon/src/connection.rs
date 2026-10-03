use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use slopwatch_protocol::{
    ClientFrame, Command, ErrorBody, ErrorCode, Reply, Request, RequestId, Response, ResponseBody,
    ServerFrame, Topic, TopicUpdate, WatchedPrsDelta, WatchedPrsUpdate,
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

        let mut deltas: Option<Deltas> = None;
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
                        send(&mut ws, &self.subscribe(&mut deltas)).await?;
                    }
                    // A command's deltas reach the client before its
                    // response, so a client that waits for the response
                    // sees what the command changed.
                    while let Some(next) = deltas.as_mut().map(|deltas| Next::from(deltas.try_recv())) {
                        let Some(frame) = self.frame(next, &mut deltas) else { break };
                        send(&mut ws, &frame).await?;
                    }
                    send(&mut ws, &ServerFrame::Response(Response { id, result })).await?;
                }
                next = recv(&mut deltas) => {
                    if let Some(frame) = self.frame(next, &mut deltas) {
                        send(&mut ws, &frame).await?;
                    }
                }
            }
        }
    }

    /// Starts or restarts the connection's subscription and returns the
    /// snapshot to send.
    fn subscribe(&self, deltas: &mut Option<Deltas>) -> ServerFrame {
        let Subscription {
            seq,
            snapshot,
            deltas: receiver,
        } = self.watching.subscribe();
        *deltas = Some(receiver);
        ServerFrame::Topic(TopicUpdate::WatchedPrs {
            seq,
            update: WatchedPrsUpdate::Snapshot(snapshot),
        })
    }

    /// The frame for `next`. A subscriber that fell too far behind to catch
    /// up starts over from a fresh snapshot.
    fn frame(&self, next: Next, deltas: &mut Option<Deltas>) -> Option<ServerFrame> {
        match next {
            Next::Delta(seq, delta) => Some(ServerFrame::Topic(TopicUpdate::WatchedPrs {
                seq,
                update: WatchedPrsUpdate::Delta(delta),
            })),
            Next::Lagged => Some(self.subscribe(deltas)),
            Next::Nothing => None,
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
}

type Deltas = broadcast::Receiver<(u64, WatchedPrsDelta)>;

/// What a subscription has next.
enum Next {
    Delta(u64, WatchedPrsDelta),
    /// The subscriber fell behind and lost deltas.
    Lagged,
    Nothing,
}

impl From<Result<(u64, WatchedPrsDelta), broadcast::error::TryRecvError>> for Next {
    fn from(received: Result<(u64, WatchedPrsDelta), broadcast::error::TryRecvError>) -> Self {
        match received {
            Ok((seq, delta)) => Next::Delta(seq, delta),
            Err(broadcast::error::TryRecvError::Lagged(_)) => Next::Lagged,
            Err(_) => Next::Nothing,
        }
    }
}

/// The next delta for a subscribed connection. Never resolves for one that
/// hasn't subscribed.
async fn recv(deltas: &mut Option<Deltas>) -> Next {
    let Some(deltas) = deltas else {
        return std::future::pending().await;
    };
    match deltas.recv().await {
        Ok((seq, delta)) => Next::Delta(seq, delta),
        Err(broadcast::error::RecvError::Lagged(_)) => Next::Lagged,
        Err(broadcast::error::RecvError::Closed) => Next::Nothing,
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
