//! Every colour and size the GUI draws with, from the prototypes the design
//! came from (#14, #13, #37), and the gpui-kit theme built from them, so
//! stock buttons and inputs match the views.
//!
//! Views take their colours from here, never from literals. [`Look`] names
//! a Step's, the Gate's or a Run's state, and the functions below give the
//! colours each state draws in.

use std::rc::Rc;

use gpui_kit::component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::*;

use slopwatch_core::GateState;

use crate::run_pane::Look;

/// A colour as the prototype writes it, `#rrggbb`.
const fn hex(rgb: u32) -> Rgba {
    Rgba {
        r: ((rgb >> 16) & 0xff) as f32 / 255.,
        g: ((rgb >> 8) & 0xff) as f32 / 255.,
        b: (rgb & 0xff) as f32 / 255.,
        a: 1.,
    }
}

// The prototype's tokens.
pub const BG: Rgba = hex(0x0f1115);
pub const PANEL: Rgba = hex(0x161a20);
pub const PANEL2: Rgba = hex(0x1c2129);
pub const LINE: Rgba = hex(0x2a313b);
pub const TEXT: Rgba = hex(0xd8dee6);
pub const DIM: Rgba = hex(0x8a95a3);
pub const MUTED: Rgba = hex(0x5d6672);
pub const PASS: Rgba = hex(0x3fb950);
pub const FAIL: Rgba = hex(0xf85149);
pub const INC: Rgba = hex(0xe3a33b);
pub const ERR: Rgba = hex(0xc678dd);
pub const RUN: Rgba = hex(0x58a6ff);
pub const ASK: Rgba = hex(0xe3a33b);
pub const WAIVED: Rgba = hex(0x39c5bb);
pub const ACCENT: Rgba = hex(0xa78bfa);

// Surfaces.
/// A sources entry or menu item under the mouse.
pub const HOVER: Rgba = hex(0x20262f);
/// The chosen sources entry, segment or menu item.
pub const CHOSEN: Rgba = hex(0x2b3442);
pub const ROW_HOVER: Rgba = hex(0x151920);
pub const ROW_CHOSEN: Rgba = hex(0x1d2330);
/// Logs, YAML and the like.
pub const WELL: Rgba = hex(0x0b0d10);
/// An advisory node, a shade under the panel.
pub const ADVISORY_BG: Rgba = hex(0x13161b);
pub const CANVAS_DOT: Rgba = hex(0x252b34);
pub const EDGE: Rgba = hex(0x56606d);
pub const LINK: Rgba = hex(0x8cb4ff);
/// The Inbox count and the Dock badge.
pub const BADGE: Rgba = hex(0xe5484d);
pub const WHITE: Rgba = hex(0xffffff);
pub const SHADE: Rgba = Rgba {
    a: 0.7,
    ..hex(0x000000)
};

// The Gate.
pub const GATE_BG: Rgba = hex(0x17142a);
pub const GATE_LINE: Rgba = hex(0x3d3570);
pub const GATE_TEXT: Rgba = hex(0xc8bcff);
/// A chosen Run history chip, a Condition chip.
pub const ACCENT_BG: Rgba = hex(0x1d1a33);
pub const PRIMARY_BG: Rgba = hex(0x2d2560);

// Tints behind and around a state's colour.
pub const PASS_BG: Rgba = hex(0x12301a);
pub const PASS_LINE: Rgba = hex(0x23582f);
pub const FAIL_BG: Rgba = hex(0x3a1416);
pub const FAIL_LINE: Rgba = hex(0x6b2427);
/// Text on a failure tint, such as a Reject button.
pub const FAIL_TEXT: Rgba = hex(0xffc1bd);
/// An Escalation's status line in the PR list.
pub const ESCALATION_TEXT: Rgba = hex(0xff9b95);
pub const INC_BG: Rgba = hex(0x2f2610);
pub const INC_LINE: Rgba = hex(0x5e4a18);
pub const ASK_BG: Rgba = hex(0x211b0c);
pub const ESCALATION_BG: Rgba = hex(0x231011);
pub const RUN_LINE: Rgba = hex(0x1f4a7a);
pub const ERR_LINE: Rgba = hex(0x6a3a7a);
pub const WAIVED_LINE: Rgba = hex(0x1f5e59);
/// A Step that hasn't started, on the strip.
pub const WAIT_DOT: Rgba = hex(0x2b323c);

// Chips.
pub const CHIP_BG: Rgba = hex(0x232a34);
pub const CHIP_TEXT: Rgba = hex(0xb9c3cf);
pub const WRITE_BG: Rgba = hex(0x33240f);
pub const WRITE_LINE: Rgba = hex(0x6a4a17);
pub const WRITE_TEXT: Rgba = hex(0xf0c27a);
pub const REQUIRED_BG: Rgba = hex(0x14262c);
pub const REQUIRED_LINE: Rgba = hex(0x24505a);
pub const REQUIRED_TEXT: Rgba = hex(0x8fd3e0);

/// The monospace face, for SHAs, paths, ids and logs.
pub const MONO: &str = "Menlo";
/// The prototype's 12px monospace.
pub const MONO_SIZE: Pixels = px(12.);
/// Small capitals over a section, such as "RUN HISTORY".
pub const LABEL_SIZE: Pixels = px(10.);
/// The PR pane's title.
pub const TITLE_SIZE: Pixels = px(17.);
/// An inspector's or a card's heading.
pub const HEADING_SIZE: Pixels = px(15.);
/// The prototype's line height, against the text size.
pub const LINE_HEIGHT: f32 = 1.4;
/// The window's rem. gpui-kit sizes text and controls in rems, and at this
/// one `text_sm` is the prototype's 13px body text and `text_xs` its 11px
/// small text.
const REM: f32 = 13. / 0.875;

/// Configures gpui-kit's theme from the tokens: dark, with the prototype's
/// panels, lines and buttons.
pub fn install(cx: &mut App) {
    let config = ThemeConfig {
        name: "slopwatch".into(),
        mode: ThemeMode::Dark,
        font_size: Some(REM),
        mono_font_family: Some(MONO.into()),
        mono_font_size: Some(f32::from(MONO_SIZE)),
        radius: Some(6),
        radius_lg: Some(10),
        shadow: Some(false),
        colors: kit_colors(),
        ..ThemeConfig::default()
    };
    Theme::update(cx, |theme| theme.apply_config(&Rc::new(config)));
}

fn kit_colors() -> ThemeConfigColors {
    let c = |color: Rgba| {
        let byte = |channel: f32| (channel * 255.).round() as u8;
        let (r, g, b, a) = (byte(color.r), byte(color.g), byte(color.b), byte(color.a));
        Some(SharedString::from(format!("#{r:02x}{g:02x}{b:02x}{a:02x}")))
    };
    let mut colors = ThemeConfigColors::default();
    colors.background = c(BG);
    colors.foreground = c(TEXT);
    colors.border = c(LINE);
    colors.input = c(LINE);
    colors.ring = c(ACCENT);
    colors.caret = c(TEXT);
    colors.selection = c(Rgba { a: 0.4, ..ACCENT });
    colors.muted = c(PANEL2);
    colors.muted_foreground = c(DIM);
    colors.accent = c(CHOSEN);
    colors.accent_foreground = c(TEXT);
    colors.popover = c(PANEL);
    colors.popover_foreground = c(TEXT);
    colors.primary = c(PRIMARY_BG);
    colors.primary_hover = c(hex(0x3a3078));
    colors.primary_active = c(hex(0x3a3078));
    colors.primary_foreground = c(TEXT);
    colors.secondary = c(PANEL2);
    colors.secondary_hover = c(CHOSEN);
    colors.secondary_active = c(CHOSEN);
    colors.secondary_foreground = c(TEXT);
    colors.button = c(PANEL2);
    colors.button_hover = c(CHOSEN);
    colors.button_active = c(CHOSEN);
    colors.button_foreground = c(TEXT);
    colors.button_success = c(PASS_BG);
    colors.button_success_hover = c(PASS_LINE);
    colors.button_success_active = c(PASS_LINE);
    colors.button_success_foreground = c(PASS);
    colors.button_danger = c(FAIL_BG);
    colors.button_danger_hover = c(FAIL_LINE);
    colors.button_danger_active = c(FAIL_LINE);
    colors.button_danger_foreground = c(FAIL_TEXT);
    colors.success = c(PASS);
    colors.success_foreground = c(BG);
    colors.danger = c(FAIL);
    colors.danger_foreground = c(FAIL_TEXT);
    colors.warning = c(INC);
    colors.warning_foreground = c(BG);
    colors.info = c(RUN);
    colors.info_foreground = c(BG);
    colors.link = c(LINK);
    colors.link_hover = c(LINK);
    colors.link_active = c(LINK);
    colors.list = c(BG);
    colors.list_hover = c(ROW_HOVER);
    colors.list_active = c(ROW_CHOSEN);
    colors.list_active_border = c(ACCENT);
    colors.scrollbar = c(Rgba { a: 0., ..BG });
    colors.scrollbar_thumb = c(LINE);
    colors.scrollbar_thumb_hover = c(MUTED);
    colors.drag_border = c(ACCENT);
    colors.drop_target = c(Rgba { a: 0.25, ..ACCENT });
    colors.overlay = c(SHADE);
    colors.title_bar = c(PANEL);
    colors.title_bar_border = c(LINE);
    colors.window_border = c(LINE);
    colors.tab_bar = c(PANEL);
    colors.tab = c(Rgba { a: 0., ..PANEL });
    colors.tab_active = c(CHOSEN);
    colors.tab_foreground = c(DIM);
    colors.tab_active_foreground = c(TEXT);
    colors
}

/// The colour a state's dot and Verdict label draw in.
pub fn color(look: Look) -> Rgba {
    match look {
        Look::Pass => PASS,
        Look::Fail => FAIL,
        Look::Inconclusive => INC,
        Look::Error => ERR,
        Look::Running => RUN,
        Look::Asking => ASK,
        Look::Waived => WAIVED,
        Look::Skipped | Look::Cancelled | Look::Waiting => MUTED,
    }
}

/// The border of a node, or of a Step row, in a state.
pub fn node_line(look: Look) -> Rgba {
    match look {
        Look::Pass => PASS_LINE,
        Look::Fail => FAIL_LINE,
        Look::Running => RUN_LINE,
        Look::Asking | Look::Inconclusive => INC_LINE,
        Look::Error => ERR_LINE,
        Look::Waived => WAIVED_LINE,
        Look::Skipped | Look::Cancelled | Look::Waiting => LINE,
    }
}

/// A Step that didn't or hasn't run draws faded on the canvas.
pub fn faded(look: Look) -> bool {
    matches!(look, Look::Skipped | Look::Cancelled | Look::Waiting)
}

/// The Gate node's or row's border in a state.
pub fn gate_line(gate: GateState) -> Rgba {
    match gate {
        GateState::Pass => PASS,
        GateState::Fail => FAIL,
        GateState::Pending => INC,
    }
}
