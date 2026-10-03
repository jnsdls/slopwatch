//! Starters: ready-made Pipelines built from the preset Library Steps,
//! offered when the developer adds a repo (Prototype: onboarding a repo).
//! Picking one copies its Steps and Gate into the draft, and the draft
//! keeps no link back to it.
//!
//! A Starter names Library Steps by `lib/<name>`, so it resolves against
//! the developer's Library at load, like any Pipeline. A Plugin or Library
//! Step this machine lacks shows on the Step instead of stopping the pick.

use serde_json::{Map, Value};

use crate::draft::Outline;
use crate::edit::Edit;

/// A Pipeline onboarding offers as a starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starter {
    /// What the protocol names it by.
    pub key: &'static str,
    pub name: &'static str,
    /// One line on what it does, for the palette.
    pub blurb: &'static str,
    /// The Pipeline file it makes.
    pub text: &'static str,
}

/// The Starters, in the order onboarding offers them.
pub const STARTERS: &[Starter] = &[
    Starter {
        key: "review-and-fix",
        name: "Review and fix",
        blurb: "Checks, judges and fixes. You merge.",
        text: include_str!("../starters/review-and-fix.yml"),
    },
    Starter {
        key: "ask-then-merge",
        name: "Ask me, then merge",
        blurb: "Everything green, then one tap from you, then slopwatch merges.",
        text: include_str!("../starters/ask-then-merge.yml"),
    },
    Starter {
        key: "hands-off",
        name: "Hands off",
        blurb: "Two reviewers, the Fix loop, and a merge when the Gate passes.",
        text: include_str!("../starters/hands-off.yml"),
    },
    Starter {
        key: "just-ci",
        name: "Just CI",
        blurb: "CI in the Gate. Build up from the Library later.",
        text: include_str!("../starters/just-ci.yml"),
    },
];

impl Starter {
    pub fn named(key: &str) -> Option<&'static Starter> {
        STARTERS.iter().find(|starter| starter.key == key)
    }

    fn outline(&self) -> Outline {
        Outline::parse(self.text).expect("every Starter parses, as tests/starters.rs checks")
    }
}

impl Outline {
    /// The edits that replace this draft's Steps and Gate with `starter`'s.
    /// Everything else in the file, such as `fix_rounds` and comments
    /// above `steps:`, stays.
    pub fn use_starter(&self, starter: &Starter) -> Vec<Edit> {
        let mut edits: Vec<Edit> = (0..self.gate().len())
            .rev()
            .map(|index| Edit::RemoveGateTerm { index })
            .collect();
        edits.extend(self.steps().iter().map(|step| Edit::RemoveStep {
            id: step.id.clone(),
        }));
        let starter = starter.outline();
        edits.extend(starter.steps().iter().map(|step| {
            let mut entry = Map::from_iter([("uses".to_owned(), Value::from(step.uses.as_str()))]);
            if !step.needs.is_empty() {
                entry.insert("needs".to_owned(), Value::from(step.needs.clone()));
            }
            if !step.with.is_empty() {
                entry.insert("with".to_owned(), Value::Object(step.with.clone()));
            }
            if let Some(when) = &step.when {
                entry.insert("when".to_owned(), when.clone());
            }
            Edit::AddStep {
                id: step.id.clone(),
                step: entry,
            }
        }));
        edits.extend(
            starter
                .gate()
                .iter()
                .map(|term| Edit::AddGateTerm { term: term.clone() }),
        );
        edits
    }
}
