use futures_util::{SinkExt, StreamExt};
use slopwatch_protocol::{
    ClientFrame, ErrorBody, ErrorCode, Refusal, RefusalReason, Request, RequestId, Response,
    ResponseBody, ServerFrame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

use crate::{Daemon, Peer};

impl Daemon {
    /// Serves one client: the WebSocket handshake, the hello exchange, then
    /// requests until the client goes away.
    pub async fn serve<S>(&self, stream: S, peer: Peer) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ws = tokio_tungstenite::accept_async(stream).await?;

        let hello = match next_text(&mut ws).await? {
            Some(text) => match serde_json::from_str::<ClientFrame>(&text) {
                Ok(ClientFrame::Hello(hello)) => Ok(hello),
                _ => Err(Refusal {
                    reason: RefusalReason::ExpectedHello,
                    message: "The first frame must be a hello.".to_owned(),
                }),
            },
            None => return Ok(()),
        };
        match hello.and_then(|hello| self.admit(&hello, peer)) {
            Ok(welcome) => send(&mut ws, &ServerFrame::Hello(welcome)).await?,
            Err(refusal) => {
                send(&mut ws, &ServerFrame::Refused(refusal)).await?;
                return ws.close(None).await;
            }
        }

        while let Some(text) = next_text(&mut ws).await? {
            if let Some(response) = self.respond(&text) {
                send(&mut ws, &ServerFrame::Response(response)).await?;
            }
        }
        Ok(())
    }

    fn respond(&self, text: &str) -> Option<Response> {
        match serde_json::from_str::<ClientFrame>(text) {
            Ok(ClientFrame::Request(Request {
                id,
                actor: _,
                command,
            })) => Some(Response {
                id,
                result: ResponseBody::Ok(self.execute(command)),
            }),
            Ok(ClientFrame::Hello(_)) => None,
            // Answer a malformed request if it has an id to answer to.
            Err(error) => Some(Response {
                id: request_id(text)?,
                result: ResponseBody::Error(ErrorBody {
                    code: ErrorCode::BadRequest,
                    message: error.to_string(),
                }),
            }),
        }
    }
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
async fn next_text<S>(ws: &mut WebSocketStream<S>) -> Result<Option<String>, Error>
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
