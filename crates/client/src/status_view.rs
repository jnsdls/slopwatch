use gpui_kit::component::ActiveTheme;
use gpui_kit::*;

use crate::link::Status;

/// The window's only view for now: whether the GUI reached the daemon.
pub struct StatusView {
    status: Status,
}

impl StatusView {
    pub fn new() -> Self {
        Self {
            status: Status::Connecting,
        }
    }

    pub fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }
}

impl Default for StatusView {
    fn default() -> Self {
        Self::new()
    }
}

/// The headline and the line under it for each status.
pub fn describe(status: &Status) -> (&'static str, String) {
    match status {
        Status::Connecting => ("Connecting", "Looking for the slopwatch daemon.".to_owned()),
        Status::Connected { daemon_build_id } => {
            ("Connected", format!("Daemon build {daemon_build_id}"))
        }
        Status::NotRunning => (
            "Daemon not running",
            "Nothing answers on the daemon's socket.".to_owned(),
        ),
        Status::Refused { message } => ("Daemon refused the connection", message.clone()),
        Status::Failed { message } => ("Can't talk to the daemon", message.clone()),
    }
}

impl Render for StatusView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let accent = match self.status {
            Status::Connecting => theme.muted_foreground,
            Status::Connected { .. } => theme.success,
            Status::NotRunning | Status::Failed { .. } => theme.danger,
            Status::Refused { .. } => theme.warning,
        };
        let (headline, detail) = describe(&self.status);

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .text_center()
            .bg(theme.background)
            .child(div().text_xl().text_color(accent).child(headline))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob brings in GPUI's `test` macro.
    use super::describe;
    use crate::link::Status;

    #[test]
    fn connected_shows_the_daemons_build_id() {
        let status = Status::Connected {
            daemon_build_id: "abc123+dirty.feed".into(),
        };

        assert_eq!(
            describe(&status),
            ("Connected", "Daemon build abc123+dirty.feed".to_owned())
        );
    }

    #[test]
    fn not_running_says_so() {
        assert_eq!(describe(&Status::NotRunning).0, "Daemon not running");
    }
}
