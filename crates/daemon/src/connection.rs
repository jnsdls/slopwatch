use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use slopwatch_protocol::{
    Actor, ClientFrame, Command, ErrorBody, ErrorCode, InboxDelta, InboxUpdate, LogKey, LogRecord,
    NotificationsDelta, NotificationsUpdate, Reply, Request, RequestId, Response, ResponseBody,
    RunId, ServerFrame, Topic, TopicUpdate, Waiver, WatchedPrsDelta, WatchedPrsUpdate,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

use crate::inbox::InboxSubscription;
use crate::notifications::NotificationsSubscription;
use crate::runs::{Journalled, Live, LiveLog, LogError, RunError, Runs, SubscribeError};
use crate::{Daemon, LibraryError, Peer, Subscription, WatchError};

/// How long a client gets to send its hello before the daemon hangs up.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// What one connection subscribed to.
#[derive(Default)]
struct Subscriptions {
    watched_prs: Option<broadcast::Receiver<(u64, WatchedPrsDelta)>>,
    inbox: Option<broadcast::Receiver<(u64, InboxDelta)>>,
    notifications: Option<broadcast::Receiver<(u64, NotificationsDelta)>>,
    /// Each subscribed Run, with the last sequence number sent for it.
    runs: HashMap<RunId, u64>,
    /// Events appended to any Run. Only subscribed Runs' get through.
    live: Option<Live>,
    /// Each subscribed Step log, with the last record sent from it.
    logs: HashMap<LogKey, u64>,
    /// Records written to any Step log. Only subscribed logs' get through.
    live_logs: Option<LiveLog>,
}

/// What a subscription has next.
enum Next {
    WatchedPrs(u64, WatchedPrsDelta),
    /// The subscriber fell behind on `watched_prs` and lost deltas.
    WatchedPrsLagged,
    Inbox(u64, InboxDelta),
    /// The subscriber fell behind on `inbox` and lost deltas.
    InboxLagged,
    Notifications(u64, NotificationsDelta),
    /// The subscriber fell behind on `notifications` and lost deltas.
    NotificationsLagged,
    Run(RunId, Journalled),
    /// The subscriber fell behind on Run events. The journal has them.
    RunsLagged,
    Log(LogKey, LogRecord),
    /// The subscriber fell behind on Step logs. The files have them.
    LogsLagged,
    Nothing,
}

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

        let mut subs = Subscriptions::default();
        loop {
            tokio::select! {
                text = next_text(&mut ws) => {
                    let Some(text) = text? else { return Ok(()) };
                    let Some((id, actor, command)) = self.parse(&text, &mut ws).await? else {
                        continue;
                    };
                    let result = match command {
                        Command::Subscribe { topic, since } => {
                            match self.subscribe(topic, since, &mut subs).await {
                                Ok(frames) => {
                                    for frame in frames {
                                        send(&mut ws, &frame).await?;
                                    }
                                    ResponseBody::Ok(Reply::Done)
                                }
                                Err(error) => ResponseBody::Error(error),
                            }
                        }
                        Command::Unsubscribe { topic } => {
                            match topic {
                                Topic::WatchedPrs => subs.watched_prs = None,
                                Topic::Inbox => subs.inbox = None,
                                Topic::Notifications => subs.notifications = None,
                                Topic::Run(run) => {
                                    subs.runs.remove(&run);
                                    if subs.runs.is_empty() {
                                        // Nothing left to filter for.
                                        subs.live = None;
                                    }
                                }
                                Topic::StepLog(key) => {
                                    subs.logs.remove(&key);
                                    if subs.logs.is_empty() {
                                        subs.live_logs = None;
                                    }
                                }
                            }
                            ResponseBody::Ok(Reply::Done)
                        }
                        command => self.execute(command, actor).await,
                    };
                    // A command's updates reach the client before its
                    // response, so a client that waits for the response
                    // sees what the command changed.
                    loop {
                        let next = try_next(&mut subs);
                        if matches!(next, Next::Nothing) {
                            break;
                        }
                        for frame in self.frames(next, &mut subs).await {
                            send(&mut ws, &frame).await?;
                        }
                    }
                    let restarting = result == ResponseBody::Ok(Reply::Restarting);
                    send(&mut ws, &ServerFrame::Response(Response { id, result })).await?;
                    if restarting {
                        self.restart.send_replace(true);
                    }
                }
                next = recv(&mut subs) => {
                    for frame in self.frames(next, &mut subs).await {
                        send(&mut ws, &frame).await?;
                    }
                }
            }
        }
    }

    /// Starts or restarts a subscription and returns the frames due now:
    /// the `watched_prs` snapshot, or a Run's events after `since`.
    async fn subscribe(
        &self,
        topic: Topic,
        since: Option<u64>,
        subs: &mut Subscriptions,
    ) -> Result<Vec<ServerFrame>, ErrorBody> {
        match topic {
            Topic::WatchedPrs => Ok(vec![self.subscribe_watched_prs(subs)]),
            Topic::Inbox => Ok(vec![self.subscribe_inbox(subs)]),
            Topic::Notifications => Ok(vec![self.subscribe_notifications(subs)]),
            Topic::Run(run) => {
                let Some(runs) = &self.runs else {
                    return Err(run_not_found(run));
                };
                let since = since.unwrap_or(0);
                let (events, live) = runs.subscribe(run, since).map_err(|error| match error {
                    SubscribeError::NotFound(run) => run_not_found(run),
                    SubscribeError::Store(error) => ErrorBody {
                        code: ErrorCode::Internal,
                        message: format!("Database error: {error}"),
                    },
                })?;
                // A receiver from an earlier subscription already sees
                // every event since; the replay covers the rest.
                subs.live.get_or_insert(live);
                subs.runs.insert(run, since);
                Ok(events
                    .into_iter()
                    .filter_map(|event| run_frame(subs, run, event))
                    .collect())
            }
            Topic::StepLog(key) => {
                let Some(runs) = &self.runs else {
                    return Err(ErrorBody {
                        code: ErrorCode::NotFound,
                        message: format!("Not found: Run {}", key.run),
                    });
                };
                let since = since.unwrap_or(0);
                let (records, live) = runs
                    .subscribe_log(&key, since)
                    .await
                    .map_err(ErrorBody::from)?;
                subs.live_logs.get_or_insert(live);
                subs.logs.insert(key.clone(), since);
                Ok(log_frame(subs, key, records).into_iter().collect())
            }
        }
    }

    fn subscribe_watched_prs(&self, subs: &mut Subscriptions) -> ServerFrame {
        let Subscription {
            seq,
            snapshot,
            deltas,
        } = self.watching.subscribe();
        subs.watched_prs = Some(deltas);
        ServerFrame::Topic(TopicUpdate::WatchedPrs {
            seq,
            update: WatchedPrsUpdate::Snapshot(snapshot),
        })
    }

    /// The `inbox` snapshot. A daemon that doesn't run Pipelines has
    /// nothing in its Inbox, ever.
    fn subscribe_inbox(&self, subs: &mut Subscriptions) -> ServerFrame {
        let Some(runs) = &self.runs else {
            return ServerFrame::Topic(TopicUpdate::Inbox {
                seq: 0,
                update: InboxUpdate::Snapshot(Default::default()),
            });
        };
        let InboxSubscription {
            seq,
            snapshot,
            deltas,
        } = runs.inbox().subscribe();
        subs.inbox = Some(deltas);
        ServerFrame::Topic(TopicUpdate::Inbox {
            seq,
            update: InboxUpdate::Snapshot(snapshot),
        })
    }

    /// The `notifications` snapshot: what no client has acked yet. A daemon
    /// that doesn't run Pipelines never notifies.
    fn subscribe_notifications(&self, subs: &mut Subscriptions) -> ServerFrame {
        let Some(runs) = &self.runs else {
            return ServerFrame::Topic(TopicUpdate::Notifications {
                seq: 0,
                update: NotificationsUpdate::Snapshot(Vec::new()),
            });
        };
        let NotificationsSubscription {
            seq,
            snapshot,
            deltas,
        } = runs.notifications().subscribe();
        subs.notifications = Some(deltas);
        ServerFrame::Topic(TopicUpdate::Notifications {
            seq,
            update: NotificationsUpdate::Snapshot(snapshot),
        })
    }

    /// The frames for `next`. A subscriber that fell too far behind to
    /// catch up starts over: from a fresh snapshot on `watched_prs`, and
    /// from the journal on a Run.
    async fn frames(&self, next: Next, subs: &mut Subscriptions) -> Vec<ServerFrame> {
        match next {
            Next::WatchedPrs(seq, delta) => vec![ServerFrame::Topic(TopicUpdate::WatchedPrs {
                seq,
                update: WatchedPrsUpdate::Delta(delta),
            })],
            Next::WatchedPrsLagged => vec![self.subscribe_watched_prs(subs)],
            Next::Inbox(seq, delta) => vec![ServerFrame::Topic(TopicUpdate::Inbox {
                seq,
                update: InboxUpdate::Delta(delta),
            })],
            Next::InboxLagged => vec![self.subscribe_inbox(subs)],
            Next::Notifications(seq, delta) => {
                vec![ServerFrame::Topic(TopicUpdate::Notifications {
                    seq,
                    update: NotificationsUpdate::Delta(delta),
                })]
            }
            Next::NotificationsLagged => vec![self.subscribe_notifications(subs)],
            Next::Run(run, event) => run_frame(subs, run, event).into_iter().collect(),
            Next::Log(key, record) => log_frame(subs, key, vec![record]).into_iter().collect(),
            Next::LogsLagged => {
                subs.live_logs = None;
                let mut frames = Vec::new();
                let Some(runs) = &self.runs else {
                    return frames;
                };
                let subscribed: Vec<(LogKey, u64)> = subs
                    .logs
                    .iter()
                    .map(|(key, &last)| (key.clone(), last))
                    .collect();
                for (key, last) in subscribed {
                    match runs.subscribe_log(&key, last).await {
                        Ok((records, live)) => {
                            subs.live_logs.get_or_insert(live);
                            frames.extend(log_frame(subs, key, records));
                        }
                        Err(error) => {
                            eprintln!(
                                "slopwatchd: can't catch a client up on log {key}: {error:?}"
                            );
                        }
                    }
                }
                frames
            }
            Next::RunsLagged => {
                subs.live = None;
                let mut frames = Vec::new();
                let Some(runs) = &self.runs else {
                    return frames;
                };
                let subscribed: Vec<(RunId, u64)> =
                    subs.runs.iter().map(|(&run, &last)| (run, last)).collect();
                for (run, last) in subscribed {
                    match runs.subscribe(run, last) {
                        Ok((events, live)) => {
                            subs.live.get_or_insert(live);
                            frames.extend(
                                events
                                    .into_iter()
                                    .filter_map(|event| run_frame(subs, run, event)),
                            );
                        }
                        Err(error) => {
                            eprintln!(
                                "slopwatchd: can't catch a client up on Run {run}: {error:?}"
                            );
                        }
                    }
                }
                frames
            }
            Next::Nothing => Vec::new(),
        }
    }

    /// Reads a request. A malformed one with an id gets an error right
    /// away, and anything else without an id is dropped.
    async fn parse<S>(
        &self,
        text: &str,
        ws: &mut WebSocketStream<S>,
    ) -> Result<Option<(RequestId, Actor, Command)>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match serde_json::from_str::<ClientFrame>(text) {
            Ok(ClientFrame::Request(Request { id, actor, command })) => {
                Ok(Some((id, actor, command)))
            }
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

    async fn execute(&self, command: Command, actor: Actor) -> ResponseBody {
        let watching = &self.watching;
        let changes_prs = matches!(
            command,
            Command::AddRepo { .. }
                | Command::Watch { .. }
                | Command::Unwatch { .. }
                | Command::Refresh
        );
        let result = match command {
            Command::Ping => Ok(Reply::Pong),
            Command::Restart => Ok(Reply::Restarting),
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
            Command::Subscribe { .. } | Command::Unsubscribe { .. } => {
                unreachable!("the connection handles its own subscriptions")
            }
            Command::ListLibrarySteps => {
                return respond(
                    self.library
                        .list()
                        .map(|steps| Reply::LibrarySteps { steps }),
                );
            }
            Command::SaveLibraryStep { step, text } => {
                let saved = self.library.save(&step, &text);
                if saved.is_ok() {
                    // The Step may be the one an invalid Pipeline lacked.
                    self.sync_runs().await;
                }
                return respond(saved.map(|()| Reply::Done));
            }
            Command::DeleteLibraryStep { step } => {
                return respond(self.library.delete(&step).map(|()| Reply::Done));
            }
            Command::CancelRun { run } => {
                return respond(match self.runs_or_refuse() {
                    Ok(runs) => runs.cancel(run).await.map(|()| Reply::Done),
                    Err(error) => Err(error),
                });
            }
            Command::RetryStep { run, step } => {
                return respond(match self.runs_or_refuse() {
                    Ok(runs) => runs.retry(run, &step, &actor).await.map(|()| Reply::Done),
                    Err(error) => Err(error),
                });
            }
            Command::AckNotifications { done } => {
                let Some(runs) = &self.runs else {
                    return ResponseBody::Ok(Reply::Done);
                };
                return respond(
                    runs.notifications()
                        .ack(&done)
                        .map(|()| Reply::Done)
                        .map_err(RunError::Store),
                );
            }
            Command::DismissEntry { entry } => {
                return respond(match self.runs_or_refuse() {
                    Ok(runs) => runs.inbox().dismiss(entry, &actor).map(|()| Reply::Done),
                    Err(error) => Err(error),
                });
            }
            Command::ReadStepLog { key, page, filter } => {
                let Some(runs) = &self.runs else {
                    return ResponseBody::Error(ErrorBody {
                        code: ErrorCode::NotFound,
                        message: format!("Not found: Run {}", key.run),
                    });
                };
                return respond(runs.read_log(key, page, filter).await.map(Reply::StepLog));
            }
            Command::WaiveStep {
                run,
                step,
                category,
                reason,
            } => {
                let waiver = Waiver {
                    category,
                    reason,
                    actor,
                };
                return respond(match self.runs_or_refuse() {
                    Ok(runs) => runs.waive(run, &step, waiver).await.map(|()| Reply::Done),
                    Err(error) => Err(error),
                });
            }
            Command::OverrideGate {
                run,
                category,
                reason,
            } => {
                let waiver = Waiver {
                    category,
                    reason,
                    actor,
                };
                return respond(match self.runs_or_refuse() {
                    Ok(runs) => runs.override_gate(run, waiver).await.map(|()| Reply::Done),
                    Err(error) => Err(error),
                });
            }
        };
        if changes_prs {
            // What changed, even with a failed poll, may start or end a Run.
            self.sync_runs().await;
        }
        respond(result)
    }
}

impl Daemon {
    /// The daemon's Runs. Only some tests build a daemon without them.
    fn runs_or_refuse(&self) -> Result<&Runs, RunError> {
        self.runs
            .as_deref()
            .ok_or_else(|| RunError::Invalid("This daemon doesn't run Pipelines".to_owned()))
    }
}

/// The frame for a Run event, if the connection subscribed to the Run and
/// hasn't had the event yet.
fn run_frame(subs: &mut Subscriptions, run: RunId, event: Journalled) -> Option<ServerFrame> {
    let last = subs.runs.get_mut(&run)?;
    if event.seq <= *last {
        return None;
    }
    *last = event.seq;
    Some(ServerFrame::Topic(TopicUpdate::Run {
        id: run,
        seq: event.seq,
        ts: event.ts,
        event: event.event,
    }))
}

/// The frame for Step log records, holding the ones the connection
/// subscribed to and hasn't had yet.
fn log_frame(
    subs: &mut Subscriptions,
    key: LogKey,
    records: Vec<LogRecord>,
) -> Option<ServerFrame> {
    let last = subs.logs.get_mut(&key)?;
    let records: Vec<LogRecord> = records
        .into_iter()
        .filter(|record| record.seq > *last)
        .collect();
    *last = records.last()?.seq;
    Some(ServerFrame::Topic(TopicUpdate::StepLog { key, records }))
}

fn run_not_found(run: RunId) -> ErrorBody {
    ErrorBody {
        code: ErrorCode::NotFound,
        message: format!("Not found: Run {run}"),
    }
}

/// What a connection's subscriptions have ready, without waiting.
fn try_next(subs: &mut Subscriptions) -> Next {
    if let Some(deltas) = &mut subs.watched_prs {
        match deltas.try_recv() {
            Ok((seq, delta)) => return Next::WatchedPrs(seq, delta),
            Err(broadcast::error::TryRecvError::Lagged(_)) => return Next::WatchedPrsLagged,
            Err(_) => {}
        }
    }
    if let Some(deltas) = &mut subs.inbox {
        match deltas.try_recv() {
            Ok((seq, delta)) => return Next::Inbox(seq, delta),
            Err(broadcast::error::TryRecvError::Lagged(_)) => return Next::InboxLagged,
            Err(_) => {}
        }
    }
    if let Some(deltas) = &mut subs.notifications {
        match deltas.try_recv() {
            Ok((seq, delta)) => return Next::Notifications(seq, delta),
            Err(broadcast::error::TryRecvError::Lagged(_)) => return Next::NotificationsLagged,
            Err(_) => {}
        }
    }
    if let Some(live) = &mut subs.live {
        match live.try_recv() {
            Ok((run, event)) => return Next::Run(run, event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => return Next::RunsLagged,
            Err(_) => {}
        }
    }
    if let Some(live) = &mut subs.live_logs {
        match live.try_recv() {
            Ok((key, record)) => return Next::Log(key, record),
            Err(broadcast::error::TryRecvError::Lagged(_)) => return Next::LogsLagged,
            Err(_) => {}
        }
    }
    Next::Nothing
}

/// The next update for a connection. Never resolves for one that hasn't
/// subscribed to anything.
async fn recv(subs: &mut Subscriptions) -> Next {
    let Subscriptions {
        watched_prs,
        inbox,
        notifications,
        live,
        live_logs,
        ..
    } = subs;
    tokio::select! {
        received = async {
            match watched_prs {
                Some(deltas) => deltas.recv().await,
                None => std::future::pending().await,
            }
        } => match received {
            Ok((seq, delta)) => Next::WatchedPrs(seq, delta),
            Err(broadcast::error::RecvError::Lagged(_)) => Next::WatchedPrsLagged,
            Err(broadcast::error::RecvError::Closed) => Next::Nothing,
        },
        received = async {
            match inbox {
                Some(deltas) => deltas.recv().await,
                None => std::future::pending().await,
            }
        } => match received {
            Ok((seq, delta)) => Next::Inbox(seq, delta),
            Err(broadcast::error::RecvError::Lagged(_)) => Next::InboxLagged,
            Err(broadcast::error::RecvError::Closed) => Next::Nothing,
        },
        received = async {
            match notifications {
                Some(deltas) => deltas.recv().await,
                None => std::future::pending().await,
            }
        } => match received {
            Ok((seq, delta)) => Next::Notifications(seq, delta),
            Err(broadcast::error::RecvError::Lagged(_)) => Next::NotificationsLagged,
            Err(broadcast::error::RecvError::Closed) => Next::Nothing,
        },
        received = async {
            match live {
                Some(live) => live.recv().await,
                None => std::future::pending().await,
            }
        } => match received {
            Ok((run, event)) => Next::Run(run, event),
            Err(broadcast::error::RecvError::Lagged(_)) => Next::RunsLagged,
            Err(broadcast::error::RecvError::Closed) => Next::Nothing,
        },
        received = async {
            match live_logs {
                Some(live) => live.recv().await,
                None => std::future::pending().await,
            }
        } => match received {
            Ok((key, record)) => Next::Log(key, record),
            Err(broadcast::error::RecvError::Lagged(_)) => Next::LogsLagged,
            Err(broadcast::error::RecvError::Closed) => Next::Nothing,
        },
    }
}

fn respond<E: Into<ErrorBody>>(result: Result<Reply, E>) -> ResponseBody {
    match result {
        Ok(reply) => ResponseBody::Ok(reply),
        Err(error) => ResponseBody::Error(error.into()),
    }
}

impl From<WatchError> for ErrorBody {
    fn from(error: WatchError) -> Self {
        let (code, message) = match error {
            WatchError::NotFound(what) => (ErrorCode::NotFound, format!("Not found: {what}")),
            WatchError::GitHub(error) => (ErrorCode::GitHub, error.to_string()),
            WatchError::Store(error) => (ErrorCode::Internal, format!("Database error: {error}")),
        };
        ErrorBody { code, message }
    }
}

impl From<RunError> for ErrorBody {
    fn from(error: RunError) -> Self {
        let (code, message) = match error {
            RunError::NotFound(message) => (ErrorCode::NotFound, message),
            RunError::Invalid(message) => (ErrorCode::Invalid, message),
            RunError::Store(error) => (ErrorCode::Internal, format!("Database error: {error}")),
        };
        ErrorBody { code, message }
    }
}

impl From<LogError> for ErrorBody {
    fn from(error: LogError) -> Self {
        let (code, message) = match error {
            LogError::NotFound(what) => (ErrorCode::NotFound, format!("Not found: {what}")),
            LogError::Pruned(at) => (
                ErrorCode::NotFound,
                format!("The Run's detail was pruned at {at}, and its Step logs with it"),
            ),
            LogError::Store(error) => (ErrorCode::Internal, format!("Database error: {error}")),
            LogError::Io(error) => (ErrorCode::Internal, format!("Can't read the log: {error}")),
        };
        ErrorBody { code, message }
    }
}

impl From<LibraryError> for ErrorBody {
    fn from(error: LibraryError) -> Self {
        let code = match &error {
            LibraryError::BadName(_) | LibraryError::Invalid(_) => ErrorCode::Invalid,
            LibraryError::NotFound(_) => ErrorCode::NotFound,
            LibraryError::Io(_) => ErrorCode::Internal,
        };
        ErrorBody {
            code,
            message: error.to_string(),
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
