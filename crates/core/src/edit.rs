//! The Pipeline editor: replays GUI edits onto the text of
//! `.slopwatch/pipeline.yml` through `yaml-edit`'s lossless tree, so only the
//! edited nodes change and hand-written comments and ordering survive
//! (ADR 0007). `tests/round_trip.rs` keeps it in agreement with [`load`].
//!
//! New collections are written in flow style (`{ uses: ci, needs: [a] }`),
//! the shape the reference Pipeline uses, one line per Step.
//!
//! `yaml-edit` 0.3.2 parses losslessly, but some of its mutations damage
//! lines next to the edit: removing a block entry eats the blank line and
//! indentation after it, removing a block sequence item pulls the comment
//! above it onto the previous item's line, removing the last item of a flow
//! sequence leaves a stray comma that a later insert turns into `[a, , b]`,
//! and appending to a flow mapping writes `{ a: b , c: d}`. The editor makes
//! those edits itself, as splices of the text at the byte ranges `yaml-edit`
//! reports, and leaves the rest to `yaml-edit`. A `yaml-edit` release that
//! fixes them lets [`remove`] and [`append_to_flow`] go, if the suite agrees.
//!
//! [`load`]: crate::load

use std::ops::Range;
use std::str::FromStr;

use rowan::ast::AstNode;
use serde_json::{Map, Value};
use yaml_edit::{Document, Lang, Mapping, Sequence, SyntaxKind, YamlFile, YamlNode};

use crate::pipeline::GATE;

type SyntaxNode = rowan::SyntaxNode<Lang>;

/// One change the editor makes to a Pipeline file.
#[derive(Debug, Clone, PartialEq)]
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
    /// Appends an edge to a Step's `needs`, creating `needs:` if needed.
    AddNeed {
        step: String,
        need: String,
    },
    /// Removes an edge from a Step's `needs`, or `needs:` itself if it was
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
    // A mutation through `yaml-edit` changes `file`; a splice returns early.
    match edit {
        Edit::AddStep { id, step } => {
            let steps = steps(&root)?;
            if steps.contains_key(id.as_str()) {
                return Err(EditError::StepExists(id.clone()));
            }
            return Ok(set(text, &steps, id, parse(&step_flow(step))));
        }
        Edit::RemoveStep { id } => {
            let steps = steps(&root)?;
            return remove_key(text, &steps, id).ok_or_else(|| EditError::UnknownStep(id.clone()));
        }
        Edit::SetKey { step, key, value } => {
            return Ok(set(text, &step_mapping(&root, step)?, key, node(value)));
        }
        Edit::RemoveKey { step, key } => {
            return remove_key(text, &step_mapping(&root, step)?, key)
                .ok_or_else(|| unknown_key(step, key));
        }
        Edit::SetWith { step, key, value } => {
            let node_ = step_mapping(&root, step)?;
            return Ok(match child_mapping(&node_, step, "with")? {
                Some(with) => set(text, &with, key, node(value)),
                None => {
                    let with = Map::from_iter([(key.clone(), value.clone())]);
                    set(text, &node_, "with", node(&Value::Object(with)))
                }
            });
        }
        Edit::RemoveWith { step, key } => {
            let node_ = step_mapping(&root, step)?;
            let with = child_mapping(&node_, step, "with")?
                .filter(|with| with.contains_key(key.as_str()))
                .ok_or_else(|| unknown_key(step, &format!("with.{key}")))?;
            return Ok(if with.len() == 1 {
                remove_key(text, &node_, "with")
            } else {
                remove_key(text, &with, key)
            }
            .expect("the key is there"));
        }
        Edit::AddNeed { step, need } => {
            let node_ = step_mapping(&root, step)?;
            let need = Value::String(need.clone());
            return Ok(match child_sequence(&node_, step, "needs")? {
                Some(needs) => push(text, &needs, &need),
                None => set(text, &node_, "needs", node(&Value::Array(vec![need]))),
            });
        }
        Edit::RemoveNeed { step, need } => {
            let node_ = step_mapping(&root, step)?;
            let unknown = || EditError::UnknownNeed {
                step: step.clone(),
                need: need.clone(),
            };
            let needs = child_sequence(&node_, step, "needs")?.ok_or_else(unknown)?;
            let index = needs
                .values()
                .position(|n| n.as_scalar().is_some_and(|s| s.as_string() == *need))
                .ok_or_else(unknown)?;
            return Ok(if needs.len() == 1 {
                remove_key(text, &node_, "needs").expect("`needs` is there")
            } else {
                remove(
                    text,
                    &sequence_entries(&needs),
                    index,
                    needs.is_flow_style(),
                )
            });
        }
        Edit::AddGateTerm { term } => {
            return Ok(match root.get(GATE) {
                None => set(text, &root, GATE, node(&Value::Array(vec![term.clone()]))),
                Some(YamlNode::Sequence(gate)) => push(text, &gate, term),
                Some(_) => return Err(EditError::Shape(GATE)),
            });
        }
        Edit::SetGateTerm { index, term } => {
            if !gate(&root)?.set(*index, node(term)) {
                return Err(EditError::UnknownGateTerm(*index));
            }
        }
        Edit::RemoveGateTerm { index } => {
            let gate = gate(&root)?;
            let entries = sequence_entries(&gate);
            if *index >= entries.len() {
                return Err(EditError::UnknownGateTerm(*index));
            }
            return Ok(remove(text, &entries, *index, gate.is_flow_style()));
        }
        Edit::SetFixRounds(Some(rounds)) => {
            return Ok(set(text, &root, "fix_rounds", parse(&rounds.to_string())));
        }
        Edit::SetFixRounds(None) => {
            return Ok(remove_key(text, &root, "fix_rounds").unwrap_or_else(|| text.to_owned()));
        }
    }
    Ok(file.to_string())
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
        Some(_) => Err(EditError::StepShape {
            step: step.to_owned(),
            key: step.to_owned(),
        }),
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

/// Sets `key` in `mapping`, replacing its value or appending it.
fn set(text: &str, mapping: &Mapping, key: &str, value: YamlNode) -> String {
    if mapping.is_flow_style() && !mapping.contains_key(key) {
        let entry = format!("{}: {value}", string(key));
        return append_to_flow(text, mapping.syntax(), &mapping_entries(mapping), &entry);
    }
    mapping.set(key, value);
    root_text(mapping.syntax())
}

/// Appends to `sequence`.
fn push(text: &str, sequence: &Sequence, value: &Value) -> String {
    if sequence.is_flow_style() {
        return append_to_flow(
            text,
            sequence.syntax(),
            &sequence_entries(sequence),
            &flow(value),
        );
    }
    // `push` adds a blank line and drops the final newline when the sequence
    // ends the file; `insert` at the end doesn't.
    sequence.insert(sequence.len(), node(value));
    root_text(sequence.syntax())
}

/// The whole file's text, after a mutation through `yaml-edit`.
fn root_text(node: &SyntaxNode) -> String {
    node.ancestors().last().expect("a root").to_string()
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
        // A block entry's range can run on over the blank lines, comments
        // and indentation before the next entry. It ends after its last line
        // with content.
        let mut end = entry.start;
        let mut offset = entry.start;
        for line in text[entry.clone()].split_inclusive('\n') {
            offset += line.len();
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                end = offset;
            }
        }
        if !text[..end].ends_with('\n') {
            end = text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
        }
        start..end
    } else if index + 1 < entries.len() {
        entry.start..range(&entries[index + 1]).start
    } else if index > 0 {
        flow_content_end(text, &entries[index - 1])..flow_content_end(text, &entries[index])
    } else {
        entry
    };
    splice(text, cut, "")
}

/// Appends `item` to a flow collection after its last entry.
fn append_to_flow(
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

/// A `yaml-edit` node for a JSON value, written in flow style. Building the
/// text and parsing it, rather than handing `yaml-edit` a `&str`, keeps plain
/// scalars plain in flow context and multi-line strings on one line.
fn node(value: &Value) -> YamlNode {
    parse(&flow(value))
}

fn parse(text: &str) -> YamlNode {
    let doc = Document::from_str(text).expect("flow YAML the editor wrote parses");
    if let Some(m) = doc.as_mapping() {
        YamlNode::Mapping(m)
    } else if let Some(s) = doc.as_sequence() {
        YamlNode::Sequence(s)
    } else {
        YamlNode::Scalar(doc.as_scalar().expect("a scalar"))
    }
}

/// The order a new Step's keys are written in. Any other key follows them.
const STEP_KEYS: &[&str] = &["uses", "needs", "with", "when", "timeout", "stall_after"];

/// A new Step as one line of flow YAML, `uses` first.
fn step_flow(step: &Map<String, Value>) -> String {
    let rank = |key: &str| {
        STEP_KEYS
            .iter()
            .position(|k| *k == key)
            .unwrap_or(usize::MAX)
    };
    let mut entries: Vec<(&String, &Value)> = step.iter().collect();
    entries.sort_by_key(|(key, _)| rank(key));
    flow_mapping(entries)
}

fn flow_mapping<'a>(entries: impl IntoIterator<Item = (&'a String, &'a Value)>) -> String {
    let entries: Vec<String> = entries
        .into_iter()
        .map(|(k, v)| format!("{}: {}", string(k), flow(v)))
        .collect();
    if entries.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", entries.join(", "))
    }
}

/// `value` as one line of flow YAML.
fn flow(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(s) => string(s),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(flow).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => flow_mapping(map),
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
