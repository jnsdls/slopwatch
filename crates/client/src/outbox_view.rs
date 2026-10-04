//! How a control shows what the [`Outbox`] knows: a spinner on a button
//! whose command is out, and the reason the daemon refused it.

use gpui_kit::component::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::*;

use crate::outbox::Outbox;
use crate::theme;

pub trait Pending {
    /// Spins and ignores clicks while `waiting`.
    fn pending(self, waiting: bool) -> Self;
}

impl Pending for Button {
    fn pending(self, waiting: bool) -> Self {
        if waiting {
            self.icon(IconName::Loader).loading(true)
        } else {
            self
        }
    }
}

/// What a list shows before its first listing: that `what` is loading, or
/// why the daemon refused the last ask, once no other is out.
pub fn loading(outbox: &Outbox, action: &str, what: &str) -> Div {
    let line = div().text_sm();
    match outbox.error(action) {
        Some(error) if !outbox.waiting(action) => line
            .text_color(theme::FAIL)
            .child(format!("Couldn't load {what}: {error}")),
        _ => line
            .text_color(theme::DIM)
            .child(format!("Loading {what}…")),
    }
}

/// Why the daemon refused the last request `action` sent, to show at the
/// control that sent it.
pub fn refusal(outbox: &Outbox, action: &str) -> Option<Div> {
    let message = outbox.error(action)?;
    Some(div().text_xs().text_color(theme::FAIL).child(message))
}
