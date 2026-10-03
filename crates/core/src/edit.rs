//! The Pipeline editor: replays GUI edits onto the text of
//! `.slopwatch/pipeline.yml` through `yaml-edit`'s lossless tree, so only the
//! edited nodes change and hand-written comments and ordering survive
//! (ADR 0007). `tests/round_trip.rs` keeps it in agreement with [`load`].
//!
//! New collections are written in flow style (`{ uses: ci, needs: [a] }`),
//! the shape the reference Pipeline uses, one line per Step.
//!
//! `yaml-edit` reads the file into a lossless tree, and the editor finds the
//! nodes to change there. It then writes each edit itself, as a splice of the
//! text at the byte ranges the tree reports, rather than through `yaml-edit`'s
//! own mutations. In 0.3.2 those damage the lines around an edit next to a
//! blank line or a comment: removing or appending a block entry eats the
//! blank line and unindents the comment after it, replacing a block value
//! does the same, removing a sequence item pulls the comment above it onto
//! the previous line, and removing the last flow item leaves a comma that a
//! later insert turns into `[a, , b]`. A release that fixes them could take
//! the splices back, if this suite agrees.
//!
//! [`load`]: crate::load

use std::ops::Range;
use std::str::FromStr;

use rowan::ast::AstNode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use yaml_edit::{Lang, Mapping, Sequence, SyntaxKind, YamlFile, YamlNode};

use crate::pipeline::GATE;

type SyntaxNode = rowan::SyntaxNode<Lang>;

/// One change the editor makes to a Pipeline file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edit {
    /// Appends a Step after the last one under `steps:`.
    AddStep {
        id: String,
        step: Map<String, Value>,
    },
    RemoveStep {
        id: String,
    },
    /// Sets one of a Step's keys (`uses`, `when`, `timeout`, ...), replacing
    /// its value or appending the key.
    SetKey {
        step: String,
        key: String,
        value: Value,
    },
    RemoveKey {
        step: String,
        key: String,
    },
    /// Sets one `with:` override, creating `with:` if the Step has none.
    SetWith {
        step: String,
        key: String,
        value: Value,
    },
    /// Removes one `with:` override, or `with:` itself if it was the last.
    RemoveWith {
        step: String,
        key: String,
    },
    /// Appends to a Step's `needs`, creating `needs:` if needed.
    AddNeed {
        step: String,
        need: String,
    },
    /// Removes one entry from a Step's `needs`, or `needs:` itself if it was
    /// the last.
    RemoveNeed {
        step: String,
        need: String,
    },
    /// Appends a term to the Gate.
    AddGateTerm {
        term: Value,
    },
    /// Replaces the Gate term at `index`.
    SetGateTerm {
        index: usize,
        term: Value,
    },
    RemoveGateTerm {
        index: usize,
    },
    /// Sets `fix_rounds`, or removes it to fall back to the default.
    SetFixRounds(Option<u32>),
}

/// Why an edit couldn't be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("the Pipeline file isn't YAML the editor can read: {0}")]
    Syntax(String),
    #[error("the Pipeline file has no `{0}` in a shape the editor can change")]
    Shape(&'static str),
    #[error("Step `{0}` isn't in the Pipeline")]
    UnknownStep(String),
    #[error("Step `{0}` is already in the Pipeline")]
    StepExists(String),
    #[error("Step `{0}` isn't a mapping of keys, so the editor can't change it")]
    StepNotMapping(String),
    #[error("Step `{step}` writes `{key}` in a shape the editor can't change")]
    StepShape { step: String, key: String },
    #[error("Step `{step}` has no `{key}`")]
    UnknownKey { step: String, key: String },
    #[error("Step `{step}` doesn't need `{need}`")]
    UnknownNeed { step: String, need: String },
    #[error("the Gate has no term {0}")]
    UnknownGateTerm(usize),
}

/// Applies `edits` in order to the text of a Pipeline file and returns the
/// new text. Lines no edit touches come back byte for byte.
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, EditError> {
    let mut text = text.to_owned();
    for edit in edits {
        text = apply(&text, edit)?;
    }
    Ok(text)
}

fn apply(text: &str, edit: &Edit) -> Result<String, EditError> {
    let file = YamlFile::from_str(text).map_err(|e| EditError::Syntax(e.to_string()))?;
    let root = file
        .documents()
        .next()
        .and_then(|doc| doc.as_mapping())
        .ok_or(EditError::Shape("top-level mapping"))?;
    let edited = match edit {
        Edit::AddStep { id, step } => {
            let steps = steps(&root)?;
            if steps.contains_key(id.as_str()) {
                return Err(EditError::StepExists(id.clone()));
            }
            let line = format!("{}: {}", string(id), new_step_yaml(step));
            if steps.is_flow_style() && steps.is_empty() {
                // A new Pipeline's `steps: {}` becomes a block mapping, one
                // Step per line.
                replace_with_block(text, &root_entry(&root, "steps"), &line)
            } else {
                set(text, &steps, id, &new_step_yaml(step))
            }
        }
        Edit::RemoveStep { id } => {
            let steps = steps(&root)?;
            if !steps.contains_key(id.as_str()) {
                return Err(EditError::UnknownStep(id.clone()));
            }
            if steps.len() == 1 && !steps.is_flow_style() {
                replace_value(text, &root_entry(&root, "steps"), "{}")
            } else {
                remove_key(text, &steps, id).expect("the Step is there")
            }
        }
        Edit::SetKey { step, key, value } => {
            set(text, &step_mapping(&root, step)?, key, &flow_style(value))
        }
        Edit::RemoveKey { step, key } => remove_key(text, &step_mapping(&root, step)?, key)
            .ok_or_else(|| unknown_key(step, key))?,
        Edit::SetWith { step, key, value } => {
            let step_node = step_mapping(&root, step)?;
            match child_mapping(&step_node, step, "with")? {
                Some(with) => set(text, &with, key, &flow_style(value)),
                None => {
                    let with = Map::from_iter([(key.clone(), value.clone())]);
                    set(text, &step_node, "with", &flow_style(&Value::Object(with)))
                }
            }
        }
        Edit::RemoveWith { step, key } => {
            let step_node = step_mapping(&root, step)?;
            let with = child_mapping(&step_node, step, "with")?
                .filter(|with| with.contains_key(key.as_str()))
                .ok_or_else(|| unknown_key(step, &format!("with.{key}")))?;
            if with.len() == 1 {
                remove_key(text, &step_node, "with")
            } else {
                remove_key(text, &with, key)
            }
            .expect("the key is there")
        }
        Edit::AddNeed { step, need } => {
            let step_node = step_mapping(&root, step)?;
            let need = Value::String(need.clone());
            match child_sequence(&step_node, step, "needs")? {
                Some(needs) => push(text, &needs, &need),
                None => set(
                    text,
                    &step_node,
                    "needs",
                    &flow_style(&Value::Array(vec![need])),
                ),
            }
        }
        Edit::RemoveNeed { step, need } => {
            let step_node = step_mapping(&root, step)?;
            let unknown = || EditError::UnknownNeed {
                step: step.clone(),
                need: need.clone(),
            };
            let needs = child_sequence(&step_node, step, "needs")?.ok_or_else(unknown)?;
            let index = needs
                .values()
                .position(|n| n.as_scalar().is_some_and(|s| s.as_string() == *need))
                .ok_or_else(unknown)?;
            if needs.len() == 1 {
                remove_key(text, &step_node, "needs").expect("`needs` is there")
            } else {
                let entries = sequence_entries(&needs);
                remove(text, &entries, index, needs.is_flow_style())
            }
        }
        Edit::AddGateTerm { term } => match root.get(GATE) {
            None => set(
                text,
                &root,
                GATE,
                &flow_style(&Value::Array(vec![term.clone()])),
            ),
            Some(YamlNode::Sequence(gate)) => push(text, &gate, term),
            Some(_) => return Err(EditError::Shape(GATE)),
        },
        Edit::SetGateTerm { index, term } => {
            let entries = sequence_entries(&gate(&root)?);
            let entry = entries
                .get(*index)
                .ok_or(EditError::UnknownGateTerm(*index))?;
            replace_value(text, entry, &flow_style(term))
        }
        Edit::RemoveGateTerm { index } => {
            let gate = gate(&root)?;
            let entries = sequence_entries(&gate);
            if *index >= entries.len() {
                return Err(EditError::UnknownGateTerm(*index));
            }
            if entries.len() == 1 && !gate.is_flow_style() {
                // `gate:` with nothing under it wouldn't load, nor take a
                // new term.
                replace_value(text, &root_entry(&root, GATE), "[]")
            } else {
                remove(text, &entries, *index, gate.is_flow_style())
            }
        }
        Edit::SetFixRounds(Some(rounds)) => set(text, &root, "fix_rounds", &rounds.to_string()),
        Edit::SetFixRounds(None) => {
            remove_key(text, &root, "fix_rounds").unwrap_or_else(|| text.to_owned())
        }
    };
    Ok(edited)
}

/// The top-level entry for `key`, which the caller has found already.
fn root_entry(root: &Mapping, key: &str) -> SyntaxNode {
    let index = root
        .entries()
        .position(|e| e.key_matches(key))
        .expect("the key is there");
    mapping_entries(root).swap_remove(index)
}

/// Replaces the value of a top-level entry written on the key's line, such
/// as `{}`, with `line` on a line of its own below the key, indented one
/// level.
fn replace_with_block(text: &str, entry: &SyntaxNode, line: &str) -> String {
    let colon = entry
        .children_with_tokens()
        .find(|c| c.kind() == SyntaxKind::COLON)
        .expect("a mapping entry has a colon");
    let after_colon = usize::from(colon.text_range().end());
    let old = entry
        .children()
        .find(|c| c.kind() == SyntaxKind::VALUE)
        .map_or(after_colon..after_colon, |v| range(&v));
    let start = range(entry).start;
    let indent = start - text[..start].rfind('\n').map_or(0, |i| i + 1);
    splice(
        text,
        after_colon..content_end(text, old),
        &format!("\n{:indent$}  {line}", ""),
    )
}

fn unknown_key(step: &str, key: &str) -> EditError {
    EditError::UnknownKey {
        step: step.to_owned(),
        key: key.to_owned(),
    }
}

fn steps(root: &Mapping) -> Result<Mapping, EditError> {
    root.get_mapping("steps").ok_or(EditError::Shape("steps"))
}

fn gate(root: &Mapping) -> Result<Sequence, EditError> {
    root.get_sequence(GATE).ok_or(EditError::Shape(GATE))
}

fn step_mapping(root: &Mapping, step: &str) -> Result<Mapping, EditError> {
    match steps(root)?.get(step) {
        Some(YamlNode::Mapping(m)) => Ok(m),
        Some(_) => Err(EditError::StepNotMapping(step.to_owned())),
        None => Err(EditError::UnknownStep(step.to_owned())),
    }
}

fn child_mapping(parent: &Mapping, step: &str, key: &str) -> Result<Option<Mapping>, EditError> {
    match parent.get(key) {
        None => Ok(None),
        Some(YamlNode::Mapping(m)) => Ok(Some(m)),
        Some(_) => Err(EditError::StepShape {
            step: step.to_owned(),
            key: key.to_owned(),
        }),
    }
}

fn child_sequence(parent: &Mapping, step: &str, key: &str) -> Result<Option<Sequence>, EditError> {
    match parent.get(key) {
        None => Ok(None),
        Some(YamlNode::Sequence(s)) => Ok(Some(s)),
        Some(_) => Err(EditError::StepShape {
            step: step.to_owned(),
            key: key.to_owned(),
        }),
    }
}

/// Sets `key` in `mapping` to the YAML `value`, replacing its value or
/// appending it.
fn set(text: &str, mapping: &Mapping, key: &str, value: &str) -> String {
    let entries = mapping_entries(mapping);
    if let Some(index) = mapping.entries().position(|e| e.key_matches(key)) {
        return replace_value(text, &entries[index], value);
    }
    let entry = format!("{}: {value}", string(key));
    if mapping.is_flow_style() {
        append_to_flow_collection(text, mapping.syntax(), &entries, &entry)
    } else {
        append_to_block_collection(text, &entries, &entry)
    }
}

/// Appends to `sequence`.
fn push(text: &str, sequence: &Sequence, value: &Value) -> String {
    let item = flow_style(value);
    let entries = sequence_entries(sequence);
    if sequence.is_flow_style() {
        append_to_flow_collection(text, sequence.syntax(), &entries, &item)
    } else {
        append_to_block_collection(text, &entries, &format!("- {item}"))
    }
}

/// Removes `key` from `mapping`, or returns `None` if it isn't there.
fn remove_key(text: &str, mapping: &Mapping, key: &str) -> Option<String> {
    let index = mapping.entries().position(|e| e.key_matches(key))?;
    Some(remove(
        text,
        &mapping_entries(mapping),
        index,
        mapping.is_flow_style(),
    ))
}

fn mapping_entries(mapping: &Mapping) -> Vec<SyntaxNode> {
    mapping.entries().map(|e| e.syntax().clone()).collect()
}

fn sequence_entries(sequence: &Sequence) -> Vec<SyntaxNode> {
    sequence
        .syntax()
        .children()
        .filter(|c| c.kind() == SyntaxKind::SEQUENCE_ENTRY)
        .collect()
}

fn range(node: &SyntaxNode) -> Range<usize> {
    let range = node.text_range();
    range.start().into()..range.end().into()
}

/// Where a flow entry's own text ends, before the comma and spaces that
/// `yaml-edit` counts as part of it.
fn flow_content_end(text: &str, entry: &SyntaxNode) -> usize {
    let range = range(entry);
    let content = text[range.clone()]
        .trim_end()
        .trim_end_matches(',')
        .trim_end();
    range.start + content.len()
}

/// Removes entry `index` of a collection. A block entry goes with its whole
/// lines, and the blank lines and comments after it stay. A flow entry goes
/// with one comma.
fn remove(text: &str, entries: &[SyntaxNode], index: usize, flow: bool) -> String {
    let entry = range(&entries[index]);
    let cut = if !flow {
        let start = text[..entry.start].rfind('\n').map_or(0, |i| i + 1);
        start..block_entry_end(text, &entries[index])
    } else if index + 1 < entries.len() {
        entry.start..range(&entries[index + 1]).start
    } else if index > 0 {
        flow_content_end(text, &entries[index - 1])..flow_content_end(text, &entries[index])
    } else {
        entry
    };
    splice(text, cut, "")
}

/// Replaces the value of a mapping entry or sequence item with `value`. A
/// value on the key's or dash's line is replaced where it stands. A block
/// value on the lines below moves up next to it.
fn replace_value(text: &str, entry: &SyntaxNode, value: &str) -> String {
    let Some(indicator) = entry
        .children_with_tokens()
        .find(|c| matches!(c.kind(), SyntaxKind::COLON | SyntaxKind::DASH))
    else {
        // A flow sequence item is all value.
        let start = range(entry).start;
        return splice(text, start..flow_content_end(text, entry), value);
    };
    let after_indicator = usize::from(indicator.text_range().end());
    let old = entry
        .children()
        .find(|c| c.kind() == SyntaxKind::VALUE)
        .and_then(|v| v.first_child())
        .or_else(|| entry.children().last())
        .map(|v| range(&v));
    match old {
        Some(old) if !text[after_indicator..old.start].contains('\n') => {
            splice(text, old.start..content_end(text, old), value)
        }
        Some(old) => splice(
            text,
            after_indicator..content_end(text, old),
            &format!(" {value}"),
        ),
        None => splice(text, after_indicator..after_indicator, &format!(" {value}")),
    }
}

/// Where the text in `range` ends, before trailing blank lines, comment-only
/// lines and whitespace.
fn content_end(text: &str, range: Range<usize>) -> usize {
    let mut end = range.start;
    let mut offset = range.start;
    for line in text[range].split_inclusive('\n') {
        let content = line.trim();
        if !content.is_empty() && !content.starts_with('#') {
            end = offset + line.trim_end().len();
        }
        offset += line.len();
    }
    end
}

/// Where a block entry ends: after its last line with content. Its range in
/// `yaml-edit` can run on over the blank lines, comments and indentation
/// before the next entry, and those belong to what follows.
fn block_entry_end(text: &str, entry: &SyntaxNode) -> usize {
    let end = content_end(text, range(entry));
    text[end..].find('\n').map_or(text.len(), |i| end + i + 1)
}

/// Appends `item` as a new line after the last entry of a block collection,
/// at the entries' indentation.
fn append_to_block_collection(text: &str, entries: &[SyntaxNode], item: &str) -> String {
    let first = range(entries.first().expect("a block collection has entries")).start;
    let indent = first - text[..first].rfind('\n').map_or(0, |i| i + 1);
    let end = block_entry_end(text, entries.last().expect("an entry"));
    let newline = if text[..end].ends_with('\n') {
        ""
    } else {
        "\n"
    };
    splice(text, end..end, &format!("{newline}{:indent$}{item}\n", ""))
}

/// Appends `item` to a flow collection after its last entry.
fn append_to_flow_collection(
    text: &str,
    collection: &SyntaxNode,
    entries: &[SyntaxNode],
    item: &str,
) -> String {
    match entries.last() {
        Some(last) => {
            let end = flow_content_end(text, last);
            splice(text, end..end, &format!(", {item}"))
        }
        None => {
            let empty = range(collection);
            let (open, close) = if text[empty.clone()].starts_with('[') {
                ("[", "]")
            } else {
                ("{ ", " }")
            };
            splice(text, empty, &format!("{open}{item}{close}"))
        }
    }
}

fn splice(text: &str, range: Range<usize>, with: &str) -> String {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..range.start]);
    out.push_str(with);
    out.push_str(&text[range.end..]);
    out
}

/// The order a new Step's keys are written in. Any other key follows them.
const STEP_KEYS: &[&str] = &[
    "uses",
    "needs",
    "with",
    "when",
    "timeout",
    "stall_after",
    "budget_usd",
];

/// A new Step as one line of flow YAML, `uses` first.
fn new_step_yaml(step: &Map<String, Value>) -> String {
    let rank = |key: &str| {
        STEP_KEYS
            .iter()
            .position(|k| *k == key)
            .unwrap_or(usize::MAX)
    };
    let mut entries: Vec<(&String, &Value)> = step.iter().collect();
    entries.sort_by_key(|(key, _)| rank(key));
    flow_style_mapping(entries)
}

fn flow_style_mapping<'a>(entries: impl IntoIterator<Item = (&'a String, &'a Value)>) -> String {
    let entries: Vec<String> = entries
        .into_iter()
        .map(|(k, v)| format!("{}: {}", string(k), flow_style(v)))
        .collect();
    if entries.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", entries.join(", "))
    }
}

/// `value` as one line of flow YAML, the way the editor writes it into the
/// file, such as `{ model: opus }` or `[ci, review]`.
pub fn flow_style(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(s) => string(s),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(flow_style).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => flow_style_mapping(map),
    }
}

/// A string as a plain scalar when it reads back as the same string, and
/// double-quoted otherwise. JSON's escapes are a subset of YAML's.
fn string(s: &str) -> String {
    let plain = s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./*".contains(c))
        && serde_saphyr::from_str::<Value>(s).ok() == Some(Value::String(s.to_owned()));
    if plain {
        s.to_owned()
    } else {
        Value::String(s.to_owned()).to_string()
    }
}

/// The text of `key`'s entry in the top-level mapping, or in `steps:` when
/// `step` is true, as the file writes it: its whole lines for a block
/// entry, unindented, or the entry alone in a flow mapping. `None` when the
/// file has no such entry or doesn't read.
pub(crate) fn entry_source(text: &str, step: bool, key: &str) -> Option<String> {
    let file = YamlFile::from_str(text).ok()?;
    let root = file.documents().next()?.as_mapping()?;
    let mapping = if step {
        root.get_mapping("steps")?
    } else {
        root
    };
    let index = mapping.entries().position(|e| e.key_matches(key))?;
    let entry = &mapping_entries(&mapping)[index];
    let start = range(entry).start;
    if mapping.is_flow_style() {
        return Some(text[start..flow_content_end(text, entry)].to_owned());
    }
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let lines = &text[line_start..block_entry_end(text, entry)];
    let indent = lines
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let unindented: Vec<&str> = lines
        .lines()
        .map(|line| line.get(indent..).unwrap_or_else(|| line.trim_start()))
        .collect();
    Some(unindented.join("\n"))
}
