//! Posting the daemon's notifications to Notification Center (ADR 0013).
//!
//! The daemon decides what deserves a banner and keeps it pending until the
//! GUI acks it. [`Poster`] takes the `notifications` topic, posts or removes
//! each banner through a [`NotificationCenter`], and returns the ack. With
//! notifications turned off it acks without posting, so nothing piles up.
//! [`SystemCenter`] is the real center, through GPUI and
//! `UNUserNotificationCenter`; tests fake it.

use slopwatch_protocol::{
    Command, Notification, NotificationId, NotificationsDelta, NotificationsUpdate, PrRef,
    TopicUpdate,
};

/// Whether macOS lets slopwatch post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Allowed,
    /// The developer hasn't been asked yet.
    NotAsked,
    /// The developer turned notifications off.
    Denied,
    /// No Notification Center to post to, such as under `cargo run`,
    /// outside an app bundle.
    Unavailable,
}

/// Notification Center as the GUI uses it. Identifiers are notification
/// ids, so a repost replaces a banner.
pub trait NotificationCenter {
    /// The permission as last read. Never blocks.
    fn permission(&self) -> Permission;

    /// Asks the developer for permission, if they haven't been asked.
    fn ask(&self);

    /// Posts a banner under identifier `id`.
    fn post(&self, id: &str, title: &str, body: &str);

    /// Removes the banner `id` from Notification Center.
    fn remove(&self, id: &str);
}

/// Follows the `notifications` topic and acts on each notification.
#[derive(Debug, Default)]
pub struct Poster {
    /// The sequence number of the last update applied. `None` until the
    /// first snapshot.
    seq: Option<u64>,
}

impl Poster {
    /// Acts on what `update` carries and returns the ack for it, if it
    /// carried anything to act on. A snapshot comes on every connect and
    /// holds only what no client acked, so everything in it gets acted on.
    pub fn apply(
        &mut self,
        update: TopicUpdate,
        center: &dyn NotificationCenter,
    ) -> Option<Command> {
        let TopicUpdate::Notifications { seq, update } = update else {
            return None;
        };
        let todo = match update {
            NotificationsUpdate::Snapshot(pending) => pending,
            NotificationsUpdate::Delta(delta) => {
                if self.seq.is_none_or(|last| seq <= last) {
                    return None;
                }
                match delta {
                    NotificationsDelta::Put { notification } => vec![notification],
                    NotificationsDelta::Done { .. } => Vec::new(),
                }
            }
        };
        self.seq = Some(seq);
        if todo.is_empty() {
            return None;
        }
        let posting = matches!(
            center.permission(),
            Permission::Allowed | Permission::NotAsked
        );
        for notification in &todo {
            match notification {
                Notification::Post { id, title, body } if posting => {
                    center.post(&id.to_string(), title, body);
                }
                Notification::Post { .. } => {}
                Notification::Retract { id } => center.remove(&id.to_string()),
            }
        }
        Some(Command::AckNotifications { done: todo })
    }
}

/// The PR a clicked banner opens, read from its identifier.
pub fn clicked(identifier: &str) -> Option<PrRef> {
    identifier.parse::<NotificationId>().ok().map(|id| id.pr)
}

/// What the Inbox pane says about notifications, if anything.
pub fn notice(permission: Permission) -> Option<&'static str> {
    match permission {
        Permission::Denied => Some(
            "Notifications are off, so slopwatch can't tell you when something needs you. \
             The Inbox and the Dock badge still work.",
        ),
        Permission::Allowed | Permission::NotAsked | Permission::Unavailable => None,
    }
}

/// System Settings at this app's notification settings.
pub fn settings_url(bundle_id: &str) -> String {
    format!("x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={bundle_id}")
}

#[cfg(target_os = "macos")]
pub use system::SystemCenter;

/// No Notification Center off macOS.
#[cfg(not(target_os = "macos"))]
pub struct SystemCenter;

#[cfg(not(target_os = "macos"))]
impl SystemCenter {
    pub fn new(_: &gpui_kit::App) -> Self {
        Self
    }

    pub fn refresh(&self) {}
}

#[cfg(not(target_os = "macos"))]
impl NotificationCenter for SystemCenter {
    fn permission(&self) -> Permission {
        Permission::Unavailable
    }
    fn ask(&self) {}
    fn post(&self, _: &str, _: &str, _: &str) {}
    fn remove(&self, _: &str) {}
}

#[cfg(target_os = "macos")]
mod system {
    use std::sync::atomic::{AtomicU8, Ordering};

    use block2::RcBlock;
    use gpui_kit::{App, SharedString, SystemNotification};
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNAuthorizationStatus, UNNotificationSettings,
        UNUserNotificationCenter,
    };

    use super::{NotificationCenter, Permission};

    /// The permission as macOS last reported it. macOS answers on a queue
    /// of its own, so it's kept here rather than waited for.
    static PERMISSION: AtomicU8 = AtomicU8::new(NOT_ASKED);
    const NOT_ASKED: u8 = 0;
    const ALLOWED: u8 = 1;
    const DENIED: u8 = 2;

    /// Notification Center through GPUI, which posts and removes banners
    /// and reports clicks, plus `UNUserNotificationCenter` for what GPUI
    /// doesn't cover: reading the permission and asking for it up front.
    pub struct SystemCenter<'a> {
        cx: &'a App,
    }

    impl<'a> SystemCenter<'a> {
        pub fn new(cx: &'a App) -> Self {
            Self { cx }
        }

        /// Asks macOS for the permission again. [`Self::permission`] has
        /// the answer once macOS gives it, in a few milliseconds.
        pub fn refresh(&self) {
            let Some(center) = center() else { return };
            let handler =
                RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
                    // SAFETY: the framework passes valid settings for the call.
                    let status = unsafe { settings.as_ref() }.authorizationStatus();
                    let permission = match status {
                        UNAuthorizationStatus::NotDetermined => NOT_ASKED,
                        UNAuthorizationStatus::Denied => DENIED,
                        _ => ALLOWED,
                    };
                    PERMISSION.store(permission, Ordering::Relaxed);
                });
            center.getNotificationSettingsWithCompletionHandler(&handler);
        }
    }

    /// `UNUserNotificationCenter` aborts the process outside an app
    /// bundle, so it's only touched inside one.
    fn center() -> Option<objc2::rc::Retained<UNUserNotificationCenter>> {
        NSBundle::mainBundle().bundleIdentifier()?;
        Some(UNUserNotificationCenter::currentNotificationCenter())
    }

    impl NotificationCenter for SystemCenter<'_> {
        fn permission(&self) -> Permission {
            if NSBundle::mainBundle().bundleIdentifier().is_none() {
                return Permission::Unavailable;
            }
            match PERMISSION.load(Ordering::Relaxed) {
                ALLOWED => Permission::Allowed,
                DENIED => Permission::Denied,
                _ => Permission::NotAsked,
            }
        }

        fn ask(&self) {
            let Some(center) = center() else { return };
            let handler = RcBlock::new(|granted: Bool, error: *mut NSError| {
                let permission = if granted.as_bool() { ALLOWED } else { DENIED };
                PERMISSION.store(permission, Ordering::Relaxed);
                // SAFETY: the framework passes null or a valid NSError.
                if let Some(error) = unsafe { error.as_ref() } {
                    eprintln!(
                        "slopwatch: asking for notification permission failed: {}",
                        error.localizedDescription()
                    );
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &handler,
            );
        }

        fn post(&self, id: &str, title: &str, body: &str) {
            self.cx.show_system_notification(SystemNotification {
                tag: SharedString::from(id.to_owned()),
                title: SharedString::from(title.to_owned()),
                body: SharedString::from(body.to_owned()),
                actions: Vec::new(),
            });
        }

        fn remove(&self, id: &str) {
            self.cx.dismiss_system_notification(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use slopwatch_protocol::{About, EntryId, RepoName, RunId};

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Post(String, String, String),
        Remove(String),
    }

    struct FakeCenter {
        permission: Permission,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeCenter {
        fn new(permission: Permission) -> Self {
            Self {
                permission,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.take()
        }
    }

    impl NotificationCenter for FakeCenter {
        fn permission(&self) -> Permission {
            self.permission
        }

        fn ask(&self) {}

        fn post(&self, id: &str, title: &str, body: &str) {
            self.calls
                .borrow_mut()
                .push(Call::Post(id.into(), title.into(), body.into()));
        }

        fn remove(&self, id: &str) {
            self.calls.borrow_mut().push(Call::Remove(id.into()));
        }
    }

    fn id(entry: u64) -> NotificationId {
        NotificationId {
            about: About::Entry(EntryId(entry)),
            pr: PrRef {
                repo: RepoName::new("o", "r"),
                number: 7,
            },
        }
    }

    fn post(entry: u64) -> Notification {
        Notification::Post {
            id: id(entry),
            title: "Not shippable".into(),
            body: "o/r#7: `ci` failed".into(),
        }
    }

    fn snapshot(seq: u64, pending: Vec<Notification>) -> TopicUpdate {
        TopicUpdate::Notifications {
            seq,
            update: NotificationsUpdate::Snapshot(pending),
        }
    }

    fn put(seq: u64, notification: Notification) -> TopicUpdate {
        TopicUpdate::Notifications {
            seq,
            update: NotificationsUpdate::Delta(NotificationsDelta::Put { notification }),
        }
    }

    fn ack(done: Vec<Notification>) -> Option<Command> {
        Some(Command::AckNotifications { done })
    }

    #[test]
    fn each_pending_notification_posts_under_its_id_and_is_acked() {
        let center = FakeCenter::new(Permission::Allowed);
        let mut poster = Poster::default();

        let acked = poster.apply(snapshot(4, vec![post(1)]), &center);
        assert_eq!(acked, ack(vec![post(1)]));
        let acked = poster.apply(put(5, post(2)), &center);
        assert_eq!(acked, ack(vec![post(2)]));

        assert_eq!(
            center.calls(),
            [
                Call::Post(
                    "inbox:1@o/r#7".into(),
                    "Not shippable".into(),
                    "o/r#7: `ci` failed".into()
                ),
                Call::Post(
                    "inbox:2@o/r#7".into(),
                    "Not shippable".into(),
                    "o/r#7: `ci` failed".into()
                ),
            ]
        );
    }

    #[test]
    fn a_retract_removes_the_banner_even_with_notifications_off() {
        let center = FakeCenter::new(Permission::Denied);
        let mut poster = Poster::default();

        let acked = poster.apply(
            snapshot(1, vec![post(1), Notification::Retract { id: id(2) }]),
            &center,
        );

        assert_eq!(
            acked,
            ack(vec![post(1), Notification::Retract { id: id(2) }]),
            "both are acked"
        );
        assert_eq!(
            center.calls(),
            [Call::Remove("inbox:2@o/r#7".into())],
            "nothing posts while notifications are off"
        );
    }

    #[test]
    fn deltas_before_the_snapshot_or_already_in_it_do_nothing() {
        let center = FakeCenter::new(Permission::NotAsked);
        let mut poster = Poster::default();

        assert_eq!(poster.apply(put(1, post(1)), &center), None);
        assert_eq!(poster.apply(snapshot(3, vec![]), &center), None);
        assert_eq!(poster.apply(put(3, post(1)), &center), None);
        let done = TopicUpdate::Notifications {
            seq: 4,
            update: NotificationsUpdate::Delta(NotificationsDelta::Done { id: id(1) }),
        };
        assert_eq!(poster.apply(done, &center), None);
        assert!(center.calls().is_empty());

        assert_eq!(
            poster.apply(put(5, post(3)), &center),
            ack(vec![post(3)]),
            "not asked yet still posts, which asks"
        );
        assert_eq!(center.calls().len(), 1);
    }

    #[test]
    fn a_click_opens_the_pr_its_id_names() {
        let shippable = NotificationId {
            about: About::Shippable(RunId(40)),
            pr: PrRef {
                repo: RepoName::new("o", "r"),
                number: 9,
            },
        };

        assert_eq!(clicked(&shippable.to_string()), Some(shippable.pr));
        assert_eq!(clicked("gpui-something-else"), None);
    }

    #[test]
    fn only_turned_off_notifications_show_a_notice() {
        assert!(
            notice(Permission::Denied)
                .unwrap()
                .contains("Notifications are off")
        );
        assert_eq!(notice(Permission::Allowed), None);
        assert_eq!(notice(Permission::NotAsked), None);
        assert_eq!(
            settings_url("com.jnsdls.slopwatch.dev"),
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=com.jnsdls.slopwatch.dev"
        );
    }
}
