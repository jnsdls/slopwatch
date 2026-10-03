//! The framework-free core of slopwatch: the Pipeline model, its load-time
//! validation, Gate and Condition evaluation, and the Outcome reuse key.
//!
//! [`load`] turns the text of `.slopwatch/pipeline.yml` into a [`Pipeline`]
//! or a list of [`LoadError`]s. [`Pipeline::gate`] and [`Pipeline::plan`]
//! answer, for a [`RunState`], what the Gate says and what each pending Step
//! does next.

mod edit;
mod error;
mod expr;
mod load;
mod pipeline;
mod reuse;
mod run;
mod validate;
mod verdict;

pub use edit::{Edit, EditError, apply_edits};
pub use error::{LoadError, Referrer};
pub use expr::{Expr, Fact, Glob, StepTerm};
pub use load::{PluginInfo, Resolver, load};
pub use pipeline::{
    BUILTIN_PLUGINS, FIX_ROUNDS_CEILING, FIX_ROUNDS_DEFAULT, GATE, Pipeline, Step, Uses, Workspace,
};
pub use reuse::ReuseKey;
pub use run::{Decision, Plan, PrFacts, RunState, SkipReason};
pub use verdict::{GateState, StepState, Verdict};
