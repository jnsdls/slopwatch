//! The framework-free core of slopwatch: the Pipeline model, its load-time
//! validation, Gate and Condition evaluation, and the Outcome reuse key.
//!
//! [`load`] turns the text of `.slopwatch/pipeline.yml` into a [`Pipeline`]
//! or a list of [`LoadError`]s. [`Pipeline::gate`] and [`Pipeline::plan`]
//! answer, for a [`RunState`], what the Gate says and what each pending Step
//! does next.

mod draft;
mod edit;
mod error;
mod expr;
mod library;
mod load;
mod pipeline;
mod replay;
mod reuse;
mod run;
mod starter;
mod validate;
mod verdict;

pub use draft::{
    EMPTY_PIPELINE, EditRefusal, GateRole, Outline, OutlineStep, Target, parse_yaml, try_edits,
};
pub use edit::{Edit, EditError, apply_edits, flow_style};
pub use error::{LoadError, Referrer};
pub use expr::{Expr, Fact, Glob, StepTerm};
pub use library::{PRESETS, Preset, check_library_step, is_library_step_name, library_step_plugin};
pub use load::{PluginInfo, Resolver, check, load, parse_duration, resolve_uses};
pub use pipeline::{
    BUILTIN_PLUGINS, FIX_ROUNDS_CEILING, FIX_ROUNDS_DEFAULT, GATE, PR_BUDGET_DEFAULT, Pipeline,
    Step, Uses, Workspace,
};
pub use replay::{Conflict, Node, ReplayError, Replayed, replay};
pub use reuse::ReuseKey;
pub use run::{Decision, Plan, PrFacts, RunState, SkipReason};
pub use starter::{STARTERS, Starter};
pub use verdict::{EndReason, GateState, StepState, Verdict, WaiverCategory};
