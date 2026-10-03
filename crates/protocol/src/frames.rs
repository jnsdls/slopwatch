use serde::{Deserialize, Serialize};

use crate::{RepoName, Topic, TopicUpdate};

/// A frame a client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(ClientHello),
    Request(Request),
}

/// A frame the daemon sends to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello(ServerHello),
    /// The daemon's last frame on a connection it won't serve. It closes
    /// the socket right after.
    Refused(Refusal),
    Response(Response),
    /// An update on a topic the client subscribed to.
    Topic(TopicUpdate),
}

/// The first frame a client sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub dialect: u32,
    /// Additive capabilities. A peer ignores the ones it doesn't know.
    pub features: Vec<String>,
    pub build_id: String,
    pub auth: Auth,
}

impl ClientHello {
    /// The dialect of a raw hello frame, read without parsing the rest, so a
    /// peer can tell a hello from another dialect even if its shape changed.
    /// `None` if the frame isn't a hello.
    pub fn peek_dialect(text: &str) -> Option<u32> {
        #[derive(Deserialize)]
        struct Peek {
            r#type: String,
            dialect: u32,
        }
        let peek: Peek = serde_json::from_str(text).ok()?;
        (peek.r#type == "hello").then_some(peek.dialect)
    }

    /// The hello a client of this build sends on the local socket.
    pub fn local() -> Self {
        Self {
            dialect: crate::DIALECT,
            features: crate::FEATURES
                .iter()
                .map(|&feature| feature.to_owned())
                .collect(),
            build_id: crate::BUILD_ID.to_owned(),
            auth: Auth::Local,
        }
    }
}

/// The daemon's answer to a hello it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub dialect: u32,
    pub features: Vec<String>,
    pub build_id: String,
}

/// How a client proves who it is. v1 accepts only `local`: the client is on
/// the daemon's Unix socket, and the daemon checks the peer's uid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Auth {
    Local,
    /// Any scheme this build doesn't know, such as a later `token`.
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub reason: RefusalReason,
    /// Text a client can show the developer as is.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The client's dialect differs from the daemon's. Restarting the
    /// daemon brings it to the client's build (ADR 0009).
    DialectMismatch,
    UnsupportedAuth,
    /// The peer on the socket runs as a different user than the daemon.
    PeerUidMismatch,
    /// The first frame wasn't a hello.
    ExpectedHello,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

/// A command from a client. The daemon answers with a [`Response`] that
/// carries the same id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub actor: Actor,
    pub command: Command,
}

/// Who issued a command, so an answer from the GUI looks the same as one
/// from any other client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// The developer, acting through the named client, such as `gui`.
    Developer { via: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Command {
    Ping,
    /// The repos the developer can push to, to pick one to add.
    ListAvailableRepos,
    /// Adds a repo. Its open PRs by the developer then show up on the
    /// `watched_prs` topic.
    AddRepo {
        repo: RepoName,
    },
    /// Watches a PR: the daemon adds the `slopwatch` label on GitHub.
    Watch {
        repo: RepoName,
        number: u64,
    },
    /// Unwatches a PR: the daemon removes the `slopwatch` label.
    Unwatch {
        repo: RepoName,
        number: u64,
    },
    /// Polls GitHub now instead of waiting for the next tick.
    Refresh,
    /// Starts sending updates on `topic`: a snapshot, then deltas in
    /// sequence order. Subscribing again restarts with a fresh snapshot.
    Subscribe {
        topic: Topic,
    },
    /// Kill every Step process group and exit, so launchd starts the binary
    /// the bundle now holds (ADR 0009). The daemon replies before it exits.
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    pub result: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBody {
    Ok(Reply),
    Error(ErrorBody),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Pong,
    /// The command was carried out. What it changed arrives on topics.
    Done,
    AvailableRepos {
        repos: Vec<RepoName>,
    },
    /// The daemon exits right after sending this.
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The frame wasn't one this dialect defines, such as an unknown command.
    BadRequest,
    /// The command named a repo or PR the daemon doesn't know.
    NotFound,
    /// GitHub couldn't be reached or refused the call.
    GitHub,
    /// The daemon failed on its side, such as its database.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn wire<T: Serialize>(frame: &T) -> Value {
        serde_json::to_value(frame).unwrap()
    }

    fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn client_hello_carries_dialect_features_build_id_and_auth() {
        let hello = ClientFrame::Hello(ClientHello {
            dialect: 1,
            features: vec!["subscriptions".into()],
            build_id: "abc123".into(),
            auth: Auth::Local,
        });
        let expected = json!({
            "type": "hello",
            "dialect": 1,
            "features": ["subscriptions"],
            "build_id": "abc123",
            "auth": { "scheme": "local" },
        });

        assert_eq!(wire(&hello), expected);
        assert_eq!(parse::<ClientFrame>(expected), hello);
    }

    #[test]
    fn an_unknown_auth_scheme_still_parses_so_the_daemon_can_refuse_it() {
        let frame: ClientFrame = parse(json!({
            "type": "hello",
            "dialect": 1,
            "features": [],
            "build_id": "abc123",
            "auth": { "scheme": "token", "token": "secret" },
        }));

        let ClientFrame::Hello(hello) = frame else {
            panic!("expected a hello, got {frame:?}");
        };
        assert_eq!(hello.auth, Auth::Unsupported);
    }

    #[test]
    fn the_dialect_reads_from_a_hello_whose_other_fields_changed_shape() {
        let future = r#"{"type":"hello","dialect":9,"auth":"something new"}"#;

        assert_eq!(ClientHello::peek_dialect(future), Some(9));
        assert_eq!(
            ClientHello::peek_dialect(r#"{"type":"request","dialect":1}"#),
            None
        );
    }

    #[test]
    fn requests_carry_an_id_and_an_actor() {
        let request = ClientFrame::Request(Request {
            id: RequestId(7),
            actor: Actor::Developer { via: "gui".into() },
            command: Command::Ping,
        });
        let expected = json!({
            "type": "request",
            "id": 7,
            "actor": { "kind": "developer", "via": "gui" },
            "command": { "name": "ping" },
        });

        assert_eq!(wire(&request), expected);
        assert_eq!(parse::<ClientFrame>(expected), request);
    }

    #[test]
    fn restart_is_a_command_answered_before_the_daemon_exits() {
        assert_eq!(wire(&Command::Restart), json!({ "name": "restart" }));
        assert_eq!(
            wire(&ResponseBody::Ok(Reply::Restarting)),
            json!({ "ok": { "reply": "restarting" } }),
        );
    }

    #[test]
    fn a_request_without_an_actor_is_rejected() {
        let frame = serde_json::from_value::<ClientFrame>(json!({
            "type": "request",
            "id": 7,
            "command": { "name": "ping" },
        }));

        assert!(frame.is_err());
    }

    #[test]
    fn responses_echo_the_request_id() {
        let ok = ServerFrame::Response(Response {
            id: RequestId(7),
            result: ResponseBody::Ok(Reply::Pong),
        });
        let error = ServerFrame::Response(Response {
            id: RequestId(8),
            result: ResponseBody::Error(ErrorBody {
                code: ErrorCode::BadRequest,
                message: "unknown command".into(),
            }),
        });

        assert_eq!(
            wire(&ok),
            json!({ "type": "response", "id": 7, "result": { "ok": { "reply": "pong" } } }),
        );
        assert_eq!(
            wire(&error),
            json!({
                "type": "response",
                "id": 8,
                "result": { "error": { "code": "bad_request", "message": "unknown command" } },
            }),
        );
    }

    #[test]
    fn a_refusal_names_its_reason() {
        let refused = ServerFrame::Refused(Refusal {
            reason: RefusalReason::DialectMismatch,
            message: "restart the daemon".into(),
        });

        assert_eq!(
            wire(&refused),
            json!({
                "type": "refused",
                "reason": "dialect_mismatch",
                "message": "restart the daemon",
            }),
        );
    }
}
