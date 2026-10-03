//! Notifications: the daemon decides what deserves one, and the GUI posts
//! it (ADR 0013).
//!
//! A new Inbox entry and a Run that ends shippable or merged each record a
//! notification. The store keeps it until a client acks it, so a restart
//! sends nothing twice and loses nothing unacked. An entry that closes
//! takes its notification back: dropped if no client posted it yet, or
//! turned into a Retract that tells the GUI to remove the banner.
//!
//! Clients follow what's pending on the `notifications` topic. With
//! notifications pending and nobody subscribed, [`launch_gui_when_unheard`]
//! starts the GUI in the background.

use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use slopwatch_protocol::{
    About, DATA_DIR_ENV, EntryId, Flavor, InboxEntry, Notification, NotificationId,
    NotificationsDelta, PrRef, RunId,
};
use tokio::sync::{Notify, broadcast};

use crate::store::{Store, StoreError};

/// Deltas a slow subscriber may fall behind by before it gets a fresh
/// snapshot instead.
const BACKLOG: usize = 256;

/// What clients still have to post or remove, as the store keeps it.
pub struct Notifications {
    state: Mutex<State>,
    /// Signalled whenever something new is pending.
    put: Notify,
}

struct State {
    store: Store,
    /// What clients still have to act on, oldest first.
    pending: Vec<Notification>,
    seq: u64,
    deltas: broadcast::Sender<(u64, NotificationsDelta)>,
}

/// A subscriber's view of the `notifications` topic: what's pending as of
/// `seq`, then every delta after it.
pub struct NotificationsSubscription {
    pub seq: u64,
    pub snapshot: Vec<Notification>,
    pub deltas: broadcast::Receiver<(u64, NotificationsDelta)>,
}

impl Notifications {
    /// Loads what was still pending when the daemon last stopped.
    pub(crate) fn load(store: Store) -> Result<Self, StoreError> {
        let pending = store.pending_notifications()?;
        Ok(Self {
            state: Mutex::new(State {
                store,
                pending,
                seq: 0,
                deltas: broadcast::channel(BACKLOG).0,
            }),
            put: Notify::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no panics while holding the notifications")
    }

    /// Follows the `notifications` topic. While anyone does, the daemon
    /// launches no GUI.
    pub fn subscribe(&self) -> NotificationsSubscription {
        let state = self.state();
        NotificationsSubscription {
            seq: state.seq,
            snapshot: state.pending.clone(),
            deltas: state.deltas.subscribe(),
        }
    }

    /// True while something is pending and no client is subscribed to
    /// hear it.
    pub fn unheard(&self) -> bool {
        let state = self.state();
        !state.pending.is_empty() && state.deltas.receiver_count() == 0
    }

    /// A client acted on these, or chose not to. An acked Post about an
    /// Inbox entry is remembered, so the entry's closing can retract it.
    pub fn ack(&self, done: &[Notification]) -> Result<(), StoreError> {
        let mut state = self.state();
        for acked in done {
            let id = acked.id();
            let Some(row) = state.store.notification(id)? else {
                continue;
            };
            // A Retract may have replaced the Post this acks.
            if row.pending.as_ref() != Some(acked) {
                continue;
            }
            match (acked, row.entry) {
                (Notification::Post { .. }, Some(_)) => {
                    state.store.set_pending_notification(id, None)?;
                }
                _ => state.store.delete_notification(id)?,
            }
            state.done(id);
        }
        Ok(())
    }

    /// A new Inbox entry deserves a banner. A cause names the first PR it
    /// holds back, which a click opens.
    pub(crate) fn entry_opened(&self, entry: &InboxEntry) -> Result<(), StoreError> {
        let Some(pr) = entry.prs.first() else {
            return Ok(());
        };
        let held: Vec<String> = entry.prs.iter().map(ToString::to_string).collect();
        let mut body = held.join(", ");
        if !entry.reasons.is_empty() {
            body = format!("{body}: {}", entry.reasons.join("; "));
        }
        let post = Notification::Post {
            id: NotificationId {
                about: About::Entry(entry.id),
                pr: pr.clone(),
            },
            title: entry.title.clone(),
            body,
        };
        self.record(post, Some(entry.id))
    }

    /// Inbox entry `entry` closed, so its banner goes. A Post no client
    /// could have seen is dropped and never posts. Otherwise a Retract
    /// replaces it, since a subscribed GUI may have posted it without its
    /// ack arriving yet.
    pub(crate) fn entry_closed(&self, entry: EntryId) -> Result<(), StoreError> {
        let mut state = self.state();
        let Some(row) = state.store.entry_notification(entry)? else {
            return Ok(());
        };
        let heard = state.deltas.receiver_count() > 0;
        match row.pending {
            Some(Notification::Post { .. }) if !heard => {
                state.store.delete_notification(&row.id)?;
                state.done(&row.id);
            }
            Some(Notification::Retract { .. }) => {}
            Some(Notification::Post { .. }) | None => {
                let retract = Notification::Retract { id: row.id };
                state
                    .store
                    .set_pending_notification(retract.id(), Some(&retract))?;
                state.put(retract);
                self.put.notify_one();
            }
        }
        Ok(())
    }

    /// Run `run` on `pr` ended shippable. `title` is the PR's, if the
    /// daemon knows it.
    pub(crate) fn shippable(
        &self,
        run: RunId,
        pr: PrRef,
        title: Option<&str>,
    ) -> Result<(), StoreError> {
        let body = match title {
            Some(title) => format!("{pr}: {title}"),
            None => pr.to_string(),
        };
        let post = Notification::Post {
            id: NotificationId {
                about: About::Shippable(run),
                pr,
            },
            title: "Shippable".to_owned(),
            body,
        };
        self.record(post, None)
    }

    fn record(&self, post: Notification, entry: Option<EntryId>) -> Result<(), StoreError> {
        let mut state = self.state();
        if state.store.insert_notification(&post, entry)? {
            state.put(post);
            self.put.notify_one();
        }
        Ok(())
    }
}

impl State {
    fn put(&mut self, notification: Notification) {
        self.pending.retain(|held| held.id() != notification.id());
        self.pending.push(notification.clone());
        self.publish(NotificationsDelta::Put { notification });
    }

    fn done(&mut self, id: &NotificationId) {
        self.pending.retain(|held| held.id() != id);
        self.publish(NotificationsDelta::Done { id: id.clone() });
    }

    fn publish(&mut self, delta: NotificationsDelta) {
        self.seq += 1;
        // No subscribers is fine: the next one starts from a snapshot.
        let _ = self.deltas.send((self.seq, delta));
    }
}

/// Starts the GUI so it can post what's pending.
pub trait Launcher: Send + Sync {
    /// Starts the GUI in the background. Returns without waiting for it.
    fn launch(&self);
}

/// Launches this flavor's app with `open -g -j`: in the background,
/// hidden, and told by `--background` to open no window (ADR 0013). A
/// daemon on an overridden data dir hands the override on, so the GUI
/// finds this daemon's socket.
///
/// `open` on an app that's already running reopens it instead, which shows
/// its window and takes focus. So a GUI that runs from this bundle but
/// hasn't subscribed yet, such as one still handing off to a new daemon,
/// is left to subscribe on its own.
pub struct OpenApp;

impl Launcher for OpenApp {
    fn launch(&self) {
        let bundle_id = Flavor::CURRENT.bundle_id();
        if gui_running() {
            return;
        }
        eprintln!("slopwatchd: launching {bundle_id} to post notifications");
        let mut open = std::process::Command::new("/usr/bin/open");
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
            let mut env = std::ffi::OsString::from(format!("{DATA_DIR_ENV}="));
            env.push(dir);
            open.arg("--env").arg(env);
        }
        let spawned = open
            .args(["-g", "-j", "-b", bundle_id, "--args", "--background"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn();
        match spawned {
            // `open` returns once Launch Services has the app; reap it off
            // the runtime.
            Ok(mut child) => {
                std::thread::spawn(move || match child.wait() {
                    Ok(status) if !status.success() => {
                        eprintln!("slopwatchd: `open -b {bundle_id}` failed: {status}");
                    }
                    _ => {}
                });
            }
            Err(error) => eprintln!("slopwatchd: can't run `open`: {error}"),
        }
    }
}

/// Whether the GUI next to this daemon in `Contents/MacOS` is running.
#[cfg(target_os = "macos")]
fn gui_running() -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Some(gui) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("slopwatch")))
    else {
        return false;
    };
    let mut pids = vec![0 as libc::pid_t; 8192];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer holds `bytes` bytes of pids.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let count = usize::try_from(count).unwrap_or(0).min(pids.len());
    pids[..count].iter().any(|&pid| {
        let mut path = [0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes, as passed.
        let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        let len = usize::try_from(len).unwrap_or(0);
        len > 0 && std::path::Path::new(std::ffi::OsStr::from_bytes(&path[..len])) == gui
    })
}

#[cfg(not(target_os = "macos"))]
fn gui_running() -> bool {
    false
}

/// How eagerly the daemon launches the GUI.
#[derive(Debug, Clone, Copy)]
pub struct LaunchPace {
    /// How long after start the daemon waits for a GUI that's already
    /// running to reconnect, as after a restart.
    pub grace: Duration,
    /// How long a launched GUI gets to subscribe before the next launch.
    pub retry: Duration,
}

impl LaunchPace {
    pub const DAEMON: LaunchPace = LaunchPace {
        grace: Duration::from_secs(10),
        retry: Duration::from_secs(60),
    };
}

/// Launches the GUI whenever notifications are pending and no client is
/// subscribed to them. Runs forever.
///
/// The daemon waits out `pace.grace` after start before its first launch,
/// so a GUI that was connected before a restart reconnects first.
pub async fn launch_gui_when_unheard(
    notifications: Arc<Notifications>,
    launcher: Arc<dyn Launcher>,
    pace: LaunchPace,
) {
    tokio::time::sleep(pace.grace).await;
    loop {
        if notifications.unheard() {
            launcher.launch();
            tokio::time::sleep(pace.retry).await;
            continue;
        }
        // A GUI that quits with something pending is caught on the next
        // round.
        let _ = tokio::time::timeout(pace.retry, notifications.put.notified()).await;
    }
}
