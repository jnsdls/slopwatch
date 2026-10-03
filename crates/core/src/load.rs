//! Turns the text of `.slopwatch/pipeline.yml` into a validated [`Pipeline`].

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::LoadError;
use crate::expr::{Context, Expr, RESERVED_IDS};
use crate::library::{self, LibraryFile, is_library_step_name};
use crate::pipeline::{
    BUILTIN_PLUGINS, FIX_ROUNDS_CEILING, FIX_ROUNDS_DEFAULT, Pipeline, Step, Uses, Workspace,
};
use crate::validate;

/// Where the loader finds what the Pipeline file names but doesn't hold.
pub trait Resolver {
    /// The text of the Library Step `name`
    /// (`~/.config/slopwatch/steps/<name>.yml`), or `None` if there is none.
    fn library_step(&self, name: &str) -> Option<String>;

    /// What core needs to know about an installed Plugin, or `None` if no
    /// Plugin by that name is installed.
    fn plugin(&self, name: &str) -> Option<PluginInfo>;
}

/// The parts of a Plugin's manifest that loading reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInfo {
    pub workspace: Workspace,
    /// Ships with the app, as opposed to a drop-in third-party executable.
    pub builtin: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineFile {
    version: u32,
    #[serde(default)]
    fix_rounds: Option<u32>,
    steps: BTreeMap<String, StepEntry>,
    #[serde(default)]
    gate: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepEntry {
    uses: String,
    #[serde(default)]
    with: Map<String, Value>,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    when: Option<Value>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    stall_after: Option<String>,
}

/// Loads and validates a Pipeline. On failure, returns the errors found.
/// Graph rules are checked only once every Step resolves, and a cycle stops
/// the checks that need an acyclic graph.
pub fn load(text: &str, resolver: &dyn Resolver) -> Result<Pipeline, Vec<LoadError>> {
    let file: PipelineFile =
        serde_saphyr::from_str(text).map_err(|e| vec![LoadError::Syntax(e.to_string())])?;

    let mut errors = Vec::new();
    if file.version != 1 {
        return Err(vec![LoadError::Version(file.version)]);
    }
    let fix_rounds = file.fix_rounds.unwrap_or(FIX_ROUNDS_DEFAULT);
    if fix_rounds > FIX_ROUNDS_CEILING {
        errors.push(LoadError::FixRoundsCeiling(fix_rounds));
    }

    let mut steps = BTreeMap::new();
    for (id, node) in file.steps {
        match resolve_step(&id, node, resolver) {
            Ok(step) => {
                steps.insert(id, step);
            }
            Err(mut step_errors) => errors.append(&mut step_errors),
        }
    }

    let gate = if file.gate.is_empty() {
        errors.push(LoadError::EmptyGate);
        Vec::new()
    } else {
        file.gate
            .iter()
            .filter_map(|term| match Expr::parse(term, Context::Gate) {
                Ok(expr) => Some(expr),
                Err(message) => {
                    errors.push(LoadError::InvalidGate(message));
                    None
                }
            })
            .collect()
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    let order = validate::graph(&steps, &gate)?;
    Ok(Pipeline {
        steps,
        gate,
        fix_rounds,
        order,
    })
}

fn resolve_step(
    id: &str,
    node: StepEntry,
    resolver: &dyn Resolver,
) -> Result<Step, Vec<LoadError>> {
    if RESERVED_IDS.contains(&id) {
        return Err(vec![LoadError::ReservedId(id.to_owned())]);
    }
    let step = id.to_owned();

    let (uses, library) = match node.uses.strip_prefix("lib/") {
        Some(name) => {
            let library = load_library_step(id, name, resolver)?;
            (Uses::Library(name.to_owned()), Some(library))
        }
        None => (Uses::Plugin(node.uses.clone()), None),
    };
    let plugin = match &library {
        Some(lib) => lib.uses.clone(),
        None => node.uses.clone(),
    };
    let Some(info) = resolver.plugin(&plugin) else {
        return Err(vec![LoadError::UnknownPlugin { step, plugin }]);
    };
    if !info.builtin && BUILTIN_PLUGINS.contains(&plugin.as_str()) {
        return Err(vec![LoadError::ReservedPluginName { step, plugin }]);
    }

    let mut errors = Vec::new();
    let (mut config, mut timeout, mut stall_after) = match library {
        Some(lib) => (lib.with, lib.timeout, lib.stall_after),
        None => (Map::new(), None, None),
    };
    config.extend(node.with);
    timeout = node.timeout.or(timeout);
    stall_after = node.stall_after.or(stall_after);
    let timeout = duration(id, "timeout", timeout, &mut errors);
    let stall_after = duration(id, "stall_after", stall_after, &mut errors);

    let when = match node.when {
        None => None,
        Some(value) => match Expr::parse(&value, Context::Condition) {
            Ok(expr) => Some(expr),
            Err(message) => {
                errors.push(LoadError::InvalidCondition {
                    step: step.clone(),
                    message,
                });
                None
            }
        },
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    let default_condition = Step::default_condition_for(&node.needs, info.workspace);
    Ok(Step {
        id: step,
        uses,
        plugin,
        workspace: info.workspace,
        builtin: info.builtin,
        config,
        timeout,
        stall_after,
        needs: node.needs,
        when,
        default_condition,
    })
}

fn load_library_step(
    step: &str,
    name: &str,
    resolver: &dyn Resolver,
) -> Result<LibraryFile, Vec<LoadError>> {
    let text = is_library_step_name(name)
        .then(|| resolver.library_step(name))
        .flatten();
    let Some(text) = text else {
        return Err(vec![LoadError::UnknownLibraryStep {
            step: step.to_owned(),
            name: name.to_owned(),
        }]);
    };
    library::parse(&text).map_err(|message| {
        vec![LoadError::InvalidLibraryStep {
            step: step.to_owned(),
            name: name.to_owned(),
            message,
        }]
    })
}

fn duration(
    step: &str,
    key: &'static str,
    value: Option<String>,
    errors: &mut Vec<LoadError>,
) -> Option<Duration> {
    let value = value?;
    let parsed = parse_duration(&value);
    if parsed.is_none() {
        errors.push(LoadError::InvalidDuration {
            step: step.to_owned(),
            key,
            value,
        });
    }
    parsed
}

/// A duration written as `90s`, `30m` or `2h`.
pub(crate) fn parse_duration(value: &str) -> Option<Duration> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&split| split > 0)?;
    let n: u64 = value[..split].parse().ok()?;
    let unit = match &value[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    Some(Duration::from_secs(n.checked_mul(unit)?))
}
