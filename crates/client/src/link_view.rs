use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::*;

use crate::link::LinkState;

/// The window's only view for now: whether the GUI reached the daemon.
pub struct LinkView {
    state: LinkState,
}

impl LinkView {
    pub fn new() -> Self {
        Self {
            state: LinkState::Connecting,
        }
    }

    pub fn set_state(&mut self, state: LinkState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }
}

impl Default for LinkView {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Warning,
    Bad,
}

impl Tone {
    fn color(self, theme: &Theme) -> Hsla {
        match self {
            Tone::Neutral => theme.muted_foreground,
            Tone::Good => theme.success,
            Tone::Warning => theme.warning,
            Tone::Bad => theme.danger,
        }
    }
}

/// What the window says about a link state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub tone: Tone,
    pub headline: &'static str,
    pub detail: String,
}

pub fn describe(state: &LinkState) -> Description {
    let (tone, headline, detail) = match state {
        LinkState::Connecting => (
            Tone::Neutral,
            "Connecting",
            "Looking for the slopwatch daemon.".to_owned(),
        ),
        LinkState::Connected { daemon_build_id } => (
            Tone::Good,
            "Connected",
            format!("Daemon build {daemon_build_id}"),
        ),
        LinkState::NotRunning => (
            Tone::Bad,
            "Daemon not running",
            "Nothing answers on the daemon's socket.".to_owned(),
        ),
        LinkState::Refused { message } => (
            Tone::Warning,
            "Daemon refused the connection",
            message.clone(),
        ),
        LinkState::Failed { message } => (Tone::Bad, "Can't talk to the daemon", message.clone()),
    };
    Description {
        tone,
        headline,
        detail,
    }
}

impl Render for LinkView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let description = describe(&self.state);

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
            .child(
                div()
                    .text_xl()
                    .text_color(description.tone.color(theme))
                    .child(description.headline),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(description.detail),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob brings in GPUI's `test` macro.
    use super::{Tone, describe};
    use crate::link::LinkState;

    #[test]
    fn connected_shows_the_daemons_build_id() {
        let state = LinkState::Connected {
            daemon_build_id: "abc123+dirty.feed".into(),
        };

        let description = describe(&state);

        assert_eq!(description.headline, "Connected");
        assert_eq!(description.detail, "Daemon build abc123+dirty.feed");
        assert_eq!(description.tone, Tone::Good);
    }

    #[test]
    fn not_running_says_so() {
        assert_eq!(
            describe(&LinkState::NotRunning).headline,
            "Daemon not running"
        );
    }
}
