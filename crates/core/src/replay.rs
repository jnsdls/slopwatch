//! Publishing a draft (ADR 0007). A draft is the Pipeline file as it stood
//! when the draft started, plus the edits since. Publishing replays those
//! edits onto the file as it is now on the branch, so the commit changes only
//! the nodes the draft edited.
//!
//! Each edit touches one node: a Step, the Gate or `fix_rounds`. A node the
//! branch changed since the draft started, and that the draft edits too, is
//! a conflict, and the replay stops with both versions. A node both sides
//! changed to the same thing isn't one: the branch has those edits already,
//! so they're dropped. Changes are compared as the loader reads them, so a
//! comment the branch added doesn't count.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::edit::{Edit, EditError, apply_edits, entry_source};
use crate::pipeline::GATE;

/// A part of a Pipeline file that edits change as a whole.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Step(String),
    Gate,
    FixRounds,
}

impl Node {
    /// The node `edit` changes.
    pub fn of(edit: &Edit) -> Node {
        match edit {
            Edit::AddStep { id, .. } | Edit::RemoveStep { id } => Node::Step(id.clone()),
            Edit::SetKey { step, .. }
            | Edit::RemoveKey { step, .. }
            | Edit::SetWith { step, .. }
            | Edit::RemoveWith { step, .. }
            | Edit::AddNeed { step, .. }
            | Edit::RemoveNeed { step, .. } => Node::Step(step.clone()),
            Edit::AddGateTerm { .. } | Edit::SetGateTerm { .. } | Edit::RemoveGateTerm { .. } => {
                Node::Gate
            }
            Edit::SetFixRounds(_) => Node::FixRounds,
        }
    }

    /// Every node `edits` change: Steps by id, then the Gate and
    /// `fix_rounds`.
    pub fn touched(edits: &[Edit]) -> BTreeSet<Node> {
        edits.iter().map(Node::of).collect()
    }

    /// `nodes` in a sentence: "Step `a`", "Steps `a` and `b` and the
    /// Gate".
    pub fn describe(nodes: &BTreeSet<Node>) -> String {
        let steps: Vec<String> = nodes
            .iter()
            .filter_map(|node| match node {
                Node::Step(id) => Some(format!("`{id}`")),
                _ => None,
            })
            .collect();
        let mut parts = Vec::new();
        match steps.as_slice() {
            [] => {}
            [one] => parts.push(format!("Step {one}")),
            many => parts.push(format!("Steps {}", join(many))),
        }
        parts.extend(
            nodes
                .iter()
                .filter(|node| !matches!(node, Node::Step(_)))
                .map(ToString::to_string),
        );
        join(&parts)
    }

    /// The node's text in the Pipeline file `text`, as written.
    pub fn source(&self, text: &str) -> Option<String> {
        match self {
            Node::Step(id) => entry_source(text, true, id),
            Node::Gate => entry_source(text, false, GATE),
            Node::FixRounds => entry_source(text, false, "fix_rounds"),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Step(id) => write!(f, "Step `{id}`"),
            Node::Gate => f.write_str("the Gate"),
            Node::FixRounds => f.write_str("`fix_rounds`"),
        }
    }
}

/// A node the draft edits and the branch changed too, differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub node: Node,
    /// The node as the draft writes it. `None` when the draft removes it.
    pub draft: Option<String>,
    /// The node as the branch writes it now. `None` when the branch removed
    /// it.
    pub branch: Option<String>,
}

/// A replay that went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayed {
    /// The file as it is now with the draft's edits applied.
    pub text: String,
    /// The edits that applied. Edits on a node the branch already changed
    /// the same way are left out, so these apply to the file as it is now.
    pub edits: Vec<Edit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// Nodes changed on both sides. Nothing was replayed.
    Conflicts(Vec<Conflict>),
    /// An edit didn't fit the file as it is now.
    Edit(EditError),
    /// One of the files isn't a Pipeline the replay can read.
    Syntax(String),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::Conflicts(conflicts) => {
                let nodes: Vec<String> = conflicts.iter().map(|c| c.node.to_string()).collect();
                write!(
                    f,
                    "{} changed on the branch since the draft started, and the draft changes {} too",
                    join(&nodes),
                    if nodes.len() == 1 { "it" } else { "them" }
                )
            }
            ReplayError::Edit(error) => error.fmt(f),
            ReplayError::Syntax(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ReplayError {}

/// Replays `edits`, made on the Pipeline file `base`, onto `now`, the same
/// file as the branch has it now.
pub fn replay(base: &str, edits: &[Edit], now: &str) -> Result<Replayed, ReplayError> {
    let drafted = apply_edits(base, edits).map_err(ReplayError::Edit)?;
    let at_base = nodes(base)?;
    let at_now = nodes(now)?;
    let in_draft = nodes(&drafted)?;
    let mut landed = BTreeSet::new();
    let mut conflicts = Vec::new();
    for node in Node::touched(edits) {
        if at_base.get(&node) == at_now.get(&node) {
            continue;
        }
        if in_draft.get(&node) == at_now.get(&node) {
            landed.insert(node);
        } else {
            conflicts.push(Conflict {
                draft: node.source(&drafted),
                branch: node.source(now),
                node,
            });
        }
    }
    if !conflicts.is_empty() {
        return Err(ReplayError::Conflicts(conflicts));
    }
    let edits: Vec<Edit> = edits
        .iter()
        .filter(|edit| !landed.contains(&Node::of(edit)))
        .cloned()
        .collect();
    let text = apply_edits(now, &edits).map_err(ReplayError::Edit)?;
    Ok(Replayed { text, edits })
}

/// Each node of a Pipeline file, as the loader reads it.
fn nodes(text: &str) -> Result<BTreeMap<Node, Value>, ReplayError> {
    let file: Value = serde_saphyr::from_str(text)
        .map_err(|error| ReplayError::Syntax(format!("the Pipeline file doesn't read: {error}")))?;
    let mut nodes = BTreeMap::new();
    if let Some(steps) = file.get("steps").and_then(Value::as_object) {
        for (id, step) in steps {
            nodes.insert(Node::Step(id.clone()), step.clone());
        }
    }
    if let Some(gate) = file.get(GATE) {
        nodes.insert(Node::Gate, gate.clone());
    }
    if let Some(rounds) = file.get("fix_rounds") {
        nodes.insert(Node::FixRounds, rounds.clone());
    }
    Ok(nodes)
}

/// "a", "a and b", "a, b and c".
fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}
