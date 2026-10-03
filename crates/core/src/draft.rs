//! The Pipeline editor's gestures. [`Outline`] reads a draft's shape
//! without resolving anything, and each gesture on it returns the [`Edit`]s
//! that carry it out. [`try_edits`] applies them and refuses any that would
//! make the Pipeline invalid, with [`check`]'s reason, so the canvas and the
//! loader agree on what a valid Pipeline is.

use std::fmt;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value};

use crate::edit::{Edit, EditError, apply_edits};
use crate::error::LoadError;
use crate::expr::RESERVED_IDS;
use crate::load::{Resolver, check};

/// The text a repo's draft starts from when its base has no Pipeline file.
pub const EMPTY_PIPELINE: &str = "version: 1\nsteps: {}\ngate: []\n";

/// A Pipeline file's Steps and Gate as written, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    steps: Vec<OutlineStep>,
    gate: Vec<Value>,
}

/// One Step as the file writes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutlineStep {
    #[serde(skip)]
    pub id: String,
    #[serde(default)]
    pub uses: String,
    #[serde(default)]
    pub needs: Vec<String>,
    /// The Step's own `with:` overrides, not merged with a Library Step's.
    #[serde(default)]
    pub with: Map<String, Value>,
    #[serde(default)]
    pub when: Option<Value>,
}

/// How the Gate reads a Step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRole {
    /// The Gate doesn't read it.
    Advisory,
    /// A term of its own: the Step must pass.
    Required,
    /// A term of its own that also accepts `skipped`.
    RequiredOrSkipped,
    /// One of the Gate's any-of group.
    AnyOf,
    /// Only a term the editor doesn't take apart, such as `and:` inside an
    /// `or:`, reads it.
    Other,
}

/// Where a port's drag ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// Another Step, which then needs the dragged one.
    Step(&'a str),
    /// The Gate, which then requires the dragged Step.
    Gate,
    /// The Gate's any-of group.
    GateAnyOf,
}

/// Why a gesture's edits weren't taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditRefusal {
    /// The edits don't fit the file, such as removing a Step that's gone.
    Edit(EditError),
    /// The Pipeline would gain these errors.
    Invalid(Vec<LoadError>),
}

impl fmt::Display for EditRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditRefusal::Edit(error) => error.fmt(f),
            EditRefusal::Invalid(errors) => {
                let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
                f.write_str(&lines.join("\n"))
            }
        }
    }
}

impl std::error::Error for EditRefusal {}

/// Applies `edits` to a draft's `text` and returns the new text, unless the
/// Pipeline would gain an error it didn't have. An empty Gate doesn't count,
/// since every new Pipeline starts with one.
pub fn try_edits(
    text: &str,
    edits: &[Edit],
    resolver: &dyn Resolver,
) -> Result<String, EditRefusal> {
    let after = apply_edits(text, edits).map_err(EditRefusal::Edit)?;
    let before = check(text, resolver);
    let gained: Vec<LoadError> = check(&after, resolver)
        .into_iter()
        .filter(|error| *error != LoadError::EmptyGate && !before.contains(error))
        .collect();
    if gained.is_empty() {
        Ok(after)
    } else {
        Err(EditRefusal::Invalid(gained))
    }
}

/// Reads a value the developer typed as YAML, such as `{ model: opus }`
/// for `with:` or `{ files: "docs/**" }` for a Condition.
pub fn parse_yaml(text: &str) -> Result<Value, String> {
    serde_saphyr::from_str(text).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    steps: Option<Ordered<OutlineStep>>,
    #[serde(default)]
    gate: Option<Vec<Value>>,
}

/// A mapping read in file order.
struct Ordered<V>(Vec<(String, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for Entries<V> {
            type Value = Ordered<V>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a mapping")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered<V>, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }

        deserializer.deserialize_map(Entries(std::marker::PhantomData))
    }
}

impl Outline {
    /// Reads the Steps and Gate of a Pipeline file. Anything else in it, and
    /// whether it would load, doesn't matter here.
    pub fn parse(text: &str) -> Result<Outline, String> {
        let file: File = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
        let steps = file
            .steps
            .map(|steps| steps.0)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, step)| OutlineStep { id, ..step })
            .collect();
        Ok(Outline {
            steps,
            gate: file.gate.unwrap_or_default(),
        })
    }

    pub fn steps(&self) -> &[OutlineStep] {
        &self.steps
    }

    pub fn step(&self, id: &str) -> Option<&OutlineStep> {
        self.steps.iter().find(|step| step.id == id)
    }

    /// The Gate's terms as written.
    pub fn gate(&self) -> &[Value] {
        &self.gate
    }

    pub fn gate_role(&self, id: &str) -> GateRole {
        let mut role = GateRole::Advisory;
        for term in &self.gate {
            match classify(term) {
                Term::Step(read, skipped) if read == id => {
                    return if skipped {
                        GateRole::RequiredOrSkipped
                    } else {
                        GateRole::Required
                    };
                }
                Term::AnyOf(items) if items.iter().any(|item| item_reads(item, id)) => {
                    role = GateRole::AnyOf;
                }
                _ if role == GateRole::Advisory && reads(term, id) => role = GateRole::Other,
                _ => {}
            }
        }
        role
    }

    /// An id for a new Step that `uses` names: the Plugin or Library Step's
    /// name, numbered when that's taken or reserved.
    pub fn new_step_id(&self, uses: &str) -> String {
        let base: String = uses
            .strip_prefix("lib/")
            .unwrap_or(uses)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let base = if base.is_empty() { "step".into() } else { base };
        let free = |id: &str| self.step(id).is_none() && !RESERVED_IDS.contains(&id);
        if free(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|id| free(id))
            .expect("some number is free")
    }

    /// Adds a Step that `uses` a Plugin or Library Step and needs `needs`.
    /// Returns its id and the edit. New Steps start outside the Gate.
    pub fn add_step(&self, uses: &str, needs: &[&str]) -> (String, Vec<Edit>) {
        let id = self.new_step_id(uses);
        let mut step = Map::from_iter([("uses".to_owned(), Value::from(uses))]);
        if !needs.is_empty() {
            step.insert("needs".to_owned(), Value::from(needs.to_vec()));
        }
        let edit = Edit::AddStep {
            id: id.clone(),
            step,
        };
        (id, vec![edit])
    }

    /// Wires the output port of `from`, a Step or the Gate, to `to`. A wire
    /// that's there already changes nothing.
    pub fn connect(&self, from: &str, to: Target) -> Vec<Edit> {
        match to {
            Target::Step(to) => match self.step(to) {
                Some(step) if !step.needs.iter().any(|need| need == from) => {
                    vec![Edit::AddNeed {
                        step: to.to_owned(),
                        need: from.to_owned(),
                    }]
                }
                _ => Vec::new(),
            },
            Target::Gate => match self.gate_role(from) {
                GateRole::Required | GateRole::RequiredOrSkipped => Vec::new(),
                _ => self.set_gate_role(from, GateRole::Required),
            },
            Target::GateAnyOf => match self.gate_role(from) {
                GateRole::AnyOf => Vec::new(),
                _ => self.set_gate_role(from, GateRole::AnyOf),
            },
        }
    }

    /// Makes the Gate read Step `id` as `role`. A term of its own keeps its
    /// place when it only changes whether it accepts `skipped`. Joining the
    /// any-of group joins the first one, or starts one.
    pub fn set_gate_role(&self, id: &str, role: GateRole) -> Vec<Edit> {
        let own = |skipped: bool| {
            if skipped {
                Value::Object(Map::from_iter([(
                    id.to_owned(),
                    Value::from(vec!["pass", "skipped"]),
                )]))
            } else {
                Value::from(id)
            }
        };
        let mut terms: Vec<Option<Value>> = Vec::with_capacity(self.gate.len());
        let mut placed = false;
        for term in &self.gate {
            terms.push(match classify(term) {
                Term::Step(read, _) if read == id => match role {
                    GateRole::Required | GateRole::RequiredOrSkipped if !placed => {
                        placed = true;
                        Some(own(role == GateRole::RequiredOrSkipped))
                    }
                    _ => None,
                },
                Term::AnyOf(items) if items.iter().any(|item| item_reads(item, id)) => {
                    let kept: Vec<Value> = items
                        .iter()
                        .filter(|item| !item_reads(item, id))
                        .cloned()
                        .collect();
                    (!kept.is_empty()).then(|| any_of(kept))
                }
                _ => Some(term.clone()),
            });
        }
        let mut added = Vec::new();
        match role {
            GateRole::Required | GateRole::RequiredOrSkipped if !placed => {
                added.push(own(role == GateRole::RequiredOrSkipped));
            }
            GateRole::AnyOf => {
                let group = terms
                    .iter_mut()
                    .flatten()
                    .find_map(|term| match classify(term) {
                        Term::AnyOf(items) => Some((term, items)),
                        _ => None,
                    });
                match group {
                    Some((term, mut items)) => {
                        items.push(Value::from(id));
                        *term = any_of(items);
                    }
                    None => added.push(any_of(vec![Value::from(id)])),
                }
            }
            _ => {}
        }
        gate_edits(&self.gate, &terms, added)
    }

    /// Flips whether Step `id`'s own Gate term accepts `skipped`.
    pub fn toggle_skipped(&self, id: &str) -> Vec<Edit> {
        match self.gate_role(id) {
            GateRole::Required => self.set_gate_role(id, GateRole::RequiredOrSkipped),
            GateRole::RequiredOrSkipped => self.set_gate_role(id, GateRole::Required),
            _ => Vec::new(),
        }
    }

    /// Removes Step `id`, the `needs` edges to it and its Gate terms. A
    /// Condition that reads it isn't rewritten, so [`try_edits`] refuses the
    /// removal and names that Condition.
    pub fn remove_step(&self, id: &str) -> Vec<Edit> {
        let mut edits: Vec<Edit> = self
            .steps
            .iter()
            .filter(|step| step.needs.iter().any(|need| need == id))
            .map(|step| Edit::RemoveNeed {
                step: step.id.clone(),
                need: id.to_owned(),
            })
            .collect();
        edits.extend(self.set_gate_role(id, GateRole::Advisory));
        edits.push(Edit::RemoveStep { id: id.to_owned() });
        edits
    }

    /// Adds `need` to Step `id`'s `needs`, or takes it out if it's there.
    pub fn toggle_need(&self, id: &str, need: &str) -> Vec<Edit> {
        let Some(step) = self.step(id) else {
            return Vec::new();
        };
        let had = step.needs.iter().any(|n| n == need);
        let (step, need) = (id.to_owned(), need.to_owned());
        if had {
            vec![Edit::RemoveNeed { step, need }]
        } else {
            vec![Edit::AddNeed { step, need }]
        }
    }

    /// Makes Step `id`'s own `with:` overrides `with`, key by key.
    pub fn set_with(&self, id: &str, with: &Map<String, Value>) -> Vec<Edit> {
        let Some(step) = self.step(id) else {
            return Vec::new();
        };
        let mut edits: Vec<Edit> = step
            .with
            .keys()
            .filter(|key| !with.contains_key(*key))
            .map(|key| Edit::RemoveWith {
                step: id.to_owned(),
                key: key.clone(),
            })
            .collect();
        edits.extend(
            with.iter()
                .filter(|(key, value)| step.with.get(*key) != Some(value))
                .map(|(key, value)| Edit::SetWith {
                    step: id.to_owned(),
                    key: key.clone(),
                    value: value.clone(),
                }),
        );
        edits
    }

    /// Sets Step `id`'s Condition, or with `None` falls back to the default.
    pub fn set_condition(&self, id: &str, when: Option<&Value>) -> Vec<Edit> {
        let Some(step) = self.step(id) else {
            return Vec::new();
        };
        match when {
            Some(when) if step.when.as_ref() != Some(when) => vec![Edit::SetKey {
                step: id.to_owned(),
                key: "when".to_owned(),
                value: when.clone(),
            }],
            None if step.when.is_some() => vec![Edit::RemoveKey {
                step: id.to_owned(),
                key: "when".to_owned(),
            }],
            _ => Vec::new(),
        }
    }

    /// Points Step `id` at another Plugin or Library Step.
    pub fn set_uses(&self, id: &str, uses: &str) -> Vec<Edit> {
        match self.step(id) {
            Some(step) if step.uses != uses => vec![Edit::SetKey {
                step: id.to_owned(),
                key: "uses".to_owned(),
                value: Value::from(uses),
            }],
            _ => Vec::new(),
        }
    }
}

/// The edits that turn the Gate's `old` terms into `new` (one per old
/// term, `None` where it goes), then append `added`. Replacements come
/// first and removals run from the back, so every index still holds.
fn gate_edits(old: &[Value], new: &[Option<Value>], added: Vec<Value>) -> Vec<Edit> {
    let mut edits = Vec::new();
    for (index, (old, new)) in old.iter().zip(new).enumerate() {
        if let Some(new) = new
            && new != old
        {
            edits.push(Edit::SetGateTerm {
                index,
                term: new.clone(),
            });
        }
    }
    for (index, new) in new.iter().enumerate().rev() {
        if new.is_none() {
            edits.push(Edit::RemoveGateTerm { index });
        }
    }
    edits.extend(added.into_iter().map(|term| Edit::AddGateTerm { term }));
    edits
}

fn any_of(items: Vec<Value>) -> Value {
    Value::Object(Map::from_iter([("or".to_owned(), Value::Array(items))]))
}

/// A Gate term as the editor sees it.
enum Term<'a> {
    /// A Step's own term, and whether it accepts `skipped`.
    Step(&'a str, bool),
    /// An `or:` group, with its items.
    AnyOf(Vec<Value>),
    Other,
}

fn classify(term: &Value) -> Term<'_> {
    if let Some((id, skipped)) = step_term(term) {
        return Term::Step(id, skipped);
    }
    match term.as_object().and_then(|map| map.get("or")) {
        Some(Value::Array(items)) if term.as_object().is_some_and(|m| m.len() == 1) => {
            Term::AnyOf(items.clone())
        }
        _ => Term::Other,
    }
}

/// `id` or `{ id: [pass, skipped] }`, a Step's term.
fn step_term(term: &Value) -> Option<(&str, bool)> {
    match term {
        Value::String(id) if !RESERVED_IDS.contains(&id.as_str()) => Some((id, false)),
        Value::Object(map) if map.len() == 1 => {
            let (id, accepted) = map.iter().next()?;
            if RESERVED_IDS.contains(&id.as_str()) {
                return None;
            }
            let skipped = match accepted {
                Value::Array(items) => items.iter().any(|item| item == "skipped"),
                Value::String(item) => item == "skipped",
                _ => return None,
            };
            Some((id, skipped))
        }
        _ => None,
    }
}

fn item_reads(item: &Value, id: &str) -> bool {
    step_term(item).is_some_and(|(read, _)| read == id)
}

/// Whether `id` appears as a Step anywhere in `term`.
fn reads(term: &Value, id: &str) -> bool {
    if item_reads(term, id) {
        return true;
    }
    match term {
        Value::Array(items) => items.iter().any(|item| reads(item, id)),
        Value::Object(map) => map.values().any(|value| reads(value, id)),
        _ => false,
    }
}
