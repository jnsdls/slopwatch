//! A client connected to a [`Daemon`] through an in-memory duplex instead of
//! a socket. The bytes in between are the real WebSocket handshake and JSON
//! frames, so nothing skips serialization. Modelled on zeron's in-process
//! RPC.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use slopwatch_protocol::{Actor, ClientFrame, Command, LOCAL_URL, Request, RequestId, ServerFrame};
use tokio::io::DuplexStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

use crate::{Daemon, Peer};

const BUFFER_BYTES: usize = 64 * 1024;

pub struct InProcessClient {
    ws: WebSocketStream<DuplexStream>,
    next_id: u64,
}

impl InProcessClient {
    /// Opens a connection to `daemon` as a peer running under the daemon's
    /// own uid, and completes the WebSocket handshake. No hello is sent.
    pub async fn connect(daemon: Arc<Daemon>) -> Result<Self, Error> {
        let uid = daemon.uid();
        Self::connect_as(daemon, Peer { uid }).await
    }

    /// Like [`connect`](Self::connect), but the daemon sees `peer` as the
    /// other end, the way the Unix transport reports a socket peer.
    pub async fn connect_as(daemon: Arc<Daemon>, peer: Peer) -> Result<Self, Error> {
        let (client_end, daemon_end) = tokio::io::duplex(BUFFER_BYTES);
        tokio::spawn(async move {
            if let Err(error) = daemon.serve(daemon_end, peer).await {
                eprintln!("slopwatchd: in-process connection failed: {error}");
            }
        });
        let (ws, _) = tokio_tungstenite::client_async(LOCAL_URL, client_end).await?;
        Ok(Self { ws, next_id: 1 })
    }

    pub async fn send(&mut self, frame: &ClientFrame) -> Result<(), Error> {
        let text = serde_json::to_string(frame).expect("client frames always serialize");
        self.send_text(text).await
    }

    /// Sends a raw text frame, for frames the protocol types can't express.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<(), Error> {
        self.ws.send(Message::text(text.into())).await
    }

    /// The next frame from the daemon, or `None` once it closed the
    /// connection.
    pub async fn recv(&mut self) -> Result<Option<ServerFrame>, Error> {
        while let Some(message) = self.ws.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let frame = serde_json::from_str(&text).expect("daemon sent a valid frame");
                    return Ok(Some(frame));
                }
                Ok(Message::Close(_)) => return Ok(None),
                Ok(_) => continue,
                Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// Sends `command` as the developer through this client and waits for
    /// the next frame.
    pub async fn request(&mut self, command: Command) -> Result<Option<ServerFrame>, Error> {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        let request = Request {
            id,
            actor: Actor::Developer {
                via: "in_process".to_owned(),
            },
            command,
        };
        self.send(&ClientFrame::Request(request)).await?;
        self.recv().await
    }
}
