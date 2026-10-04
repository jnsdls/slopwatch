use std::sync::Arc;
use std::sync::mpsc::Sender;

use gpui_kit::component::button::Button;
use gpui_kit::*;
use slopwatch_protocol::BUILD_ID;

use crate::agent::{Agent, AgentStatus};
use crate::components::ButtonLooks;
use crate::link::LinkState;
use crate::theme;

/// The window's only view for now: whether the GUI reached the daemon, and
/// a way to bring it back when it's down.
pub struct LinkView {
    state: LinkState,
    agent: Option<Arc<dyn Agent>>,
    reregister: Sender<()>,
}

impl LinkView {
    /// `agent` is `None` when the GUI runs outside its bundle.
    /// `reregister` asks the link to unregister and register the agent.
    pub fn new(agent: Option<Arc<dyn Agent>>, reregister: Sender<()>) -> Self {
        Self {
            state: LinkState::Connecting,
            agent,
            reregister,
        }
    }

    pub fn set_state(&mut self, state: LinkState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }

    fn run(&mut self, recovery: Recovery) {
        match recovery {
            Recovery::OpenLoginItems => {
                if let Some(agent) = &self.agent {
                    agent.open_login_items();
                }
            }
            // The link thread runs it: unregister blocks on its completion
            // handler, and a rebuilt agent may need a second register.
            Recovery::Reregister => {
                let _ = self.reregister.send(());
            }
        }
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
    fn color(self) -> Rgba {
        match self {
            Tone::Neutral => theme::DIM,
            Tone::Good => theme::PASS,
            Tone::Warning => theme::INC,
            Tone::Bad => theme::FAIL,
        }
    }
}

/// A way back for a daemon that isn't running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    Reregister,
    OpenLoginItems,
}

impl Recovery {
    fn label(self) -> &'static str {
        match self {
            Recovery::Reregister => "Re-register",
            Recovery::OpenLoginItems => "Open Login Items",
        }
    }
}

/// What the window says about a link state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub tone: Tone,
    pub headline: &'static str,
    pub detail: String,
    pub recoveries: Vec<Recovery>,
}

pub fn describe(state: &LinkState) -> Description {
    let mut recoveries = Vec::new();
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
        LinkState::Mismatched { daemon_build_id } => (
            Tone::Warning,
            "Daemon runs another build",
            format!(
                "Daemon build {daemon_build_id}, app build {BUILD_ID}. Reinstall slopwatch to \
                 bring them together."
            ),
        ),
        LinkState::HandingOff => (
            Tone::Neutral,
            "Restarting the daemon",
            "Registering this build's daemon with macOS. This can take half a minute.".to_owned(),
        ),
        LinkState::NotRunning { agent } => {
            let detail = match agent {
                None => "Nothing answers on the daemon's socket.",
                Some(AgentStatus::RequiresApproval) => {
                    "macOS is holding it back. Allow slopwatch in Login Items, or re-register it."
                }
                Some(_) => "Nothing answers on the daemon's socket. Re-register it to start it.",
            };
            if agent.is_some() {
                recoveries = vec![Recovery::Reregister, Recovery::OpenLoginItems];
            }
            (Tone::Bad, "Daemon not running", detail.to_owned())
        }
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
        recoveries,
    }
}

impl Render for LinkView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let description = describe(&self.state);
        let mut buttons = div().flex().gap_2().pt_2();
        for recovery in description.recoveries {
            let mut button = Button::new(recovery.label())
                .label(recovery.label())
                .on_click(cx.listener(move |view, _: &ClickEvent, _, _| {
                    view.run(recovery);
                }));
            if recovery == Recovery::Reregister {
                button = button.accent();
            }
            buttons = buttons.child(button);
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .text_center()
            .bg(theme::BG)
            .child(
                div()
                    .text_xl()
                    .text_color(description.tone.color())
                    .child(description.headline),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::DIM)
                    .child(description.detail),
            )
            .child(buttons)
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob brings in GPUI's `test` macro.
    use super::{Recovery, Tone, describe};
    use crate::agent::AgentStatus;
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
        assert!(description.recoveries.is_empty());
    }

    #[test]
    fn not_running_in_the_bundle_offers_re_register_and_login_items() {
        let state = LinkState::NotRunning {
            agent: Some(AgentStatus::NotRegistered),
        };

        let description = describe(&state);

        assert_eq!(description.headline, "Daemon not running");
        assert_eq!(
            description.recoveries,
            [Recovery::Reregister, Recovery::OpenLoginItems]
        );
    }

    #[test]
    fn an_agent_awaiting_approval_points_at_login_items() {
        let state = LinkState::NotRunning {
            agent: Some(AgentStatus::RequiresApproval),
        };

        assert!(describe(&state).detail.contains("Login Items"));
    }

    #[test]
    fn not_running_outside_the_bundle_offers_nothing_to_click() {
        let description = describe(&LinkState::NotRunning { agent: None });

        assert_eq!(description.headline, "Daemon not running");
        assert!(description.recoveries.is_empty());
    }
}
