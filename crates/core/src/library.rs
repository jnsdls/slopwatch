//! Library Steps: the format of `~/.config/slopwatch/steps/<name>.yml`, the
//! check a Library Step passes before it's saved, and the presets that ship.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::load::parse_duration;

/// A Library Step that ships with slopwatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    /// The file as a fresh Library gets it.
    pub text: &'static str,
}

/// The presets a fresh Library starts with. Their `with:` keys are read by
/// the built-in Plugins they use.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "desc-matches-diff",
        text: include_str!("../presets/desc-matches-diff.yml"),
    },
    Preset {
        name: "resolves-issue",
        text: include_str!("../presets/resolves-issue.yml"),
    },
    Preset {
        name: "claude-review",
        text: include_str!("../presets/claude-review.yml"),
    },
    Preset {
        name: "codex-review",
        text: include_str!("../presets/codex-review.yml"),
    },
    Preset {
        name: "claude-fix",
        text: include_str!("../presets/claude-fix.yml"),
    },
];

/// A Library Step file. `needs` and `when` belong only in the Pipeline.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LibraryFile {
    pub uses: String,
    #[serde(default)]
    pub with: Map<String, Value>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub stall_after: Option<String>,
}

/// Parses a Library Step file. The error says what's wrong, without naming
/// the Step.
pub(crate) fn parse(text: &str) -> Result<LibraryFile, String> {
    let library: LibraryFile = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
    if library.uses.starts_with("lib/") {
        return Err(format!(
            "it uses `{}`, but a Library Step must use a Plugin",
            library.uses
        ));
    }
    Ok(library)
}

/// Checks a Library Step's text the way loading a Pipeline that uses it
/// would, short of the Plugin being installed. The error says what's
/// wrong, without naming the Step.
pub fn check_library_step(text: &str) -> Result<(), String> {
    let library = parse(text)?;
    for (key, value) in [
        ("timeout", &library.timeout),
        ("stall_after", &library.stall_after),
    ] {
        if let Some(value) = value
            && parse_duration(value).is_none()
        {
            return Err(format!(
                "it sets `{key}: {value}`; write a duration such as 90s, 30m or 2h"
            ));
        }
    }
    Ok(())
}

/// The Plugin a Library Step's text uses, if the text reads as a Library
/// Step, whether or not that Plugin is installed.
pub fn library_step_plugin(text: &str) -> Option<String> {
    parse(text).ok().map(|library| library.uses)
}

/// Whether `name` can name a Library Step: lowercase ASCII letters, digits,
/// `-` and `_`, starting with a letter or digit. That keeps `lib/<name>` to
/// one plain file in the Library whatever a Pipeline writes, and two names
/// can't land on the same file on a case-insensitive disk.
pub fn is_library_step_name(name: &str) -> bool {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.starts_with(plain) && name.chars().all(|c| plain(c) || c == '-' || c == '_')
}
