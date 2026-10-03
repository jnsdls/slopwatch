use std::fmt;

/// One reason a Pipeline failed to load. Its message names the Step at fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Syntax(String),
    #[error("unsupported Pipeline version {0}; this build reads version 1")]
    Version(u32),
    #[error("`fix_rounds` is {0}, above its ceiling of {ceiling}", ceiling = crate::pipeline::FIX_ROUNDS_CEILING)]
    FixRoundsCeiling(u32),
    #[error("Step `{0}` can't use that id, because the Pipeline language reserves it")]
    ReservedId(String),
    #[error("Step `{step}` uses Library Step `{name}`, which doesn't exist")]
    UnknownLibraryStep { step: String, name: String },
    #[error("Step `{step}` uses Library Step `{name}`, which is invalid: {message}")]
    InvalidLibraryStep {
        step: String,
        name: String,
        message: String,
    },
    #[error("Step `{step}` uses Plugin `{plugin}`, which isn't installed")]
    UnknownPlugin { step: String, plugin: String },
    #[error(
        "Step `{step}` uses a third-party Plugin named `{plugin}`, a name reserved for a built-in Plugin"
    )]
    ReservedPluginName { step: String, plugin: String },
    #[error("Step `{step}` sets `{key}: {value}`; write a duration such as 90s, 30m or 2h")]
    InvalidDuration {
        step: String,
        key: &'static str,
        value: String,
    },
    #[error("Step `{step}` has an invalid Condition: {message}")]
    InvalidCondition { step: String, message: String },
    #[error("the Gate is invalid: {0}")]
    InvalidGate(String),
    #[error("the Gate references no Steps")]
    EmptyGate,
    #[error("Step `{step}` needs `{target}`, which isn't a Step")]
    UnknownNeed { step: String, target: String },
    #[error("{referrer} references `{target}`, which isn't a Step")]
    UnknownReference { referrer: Referrer, target: String },
    #[error("the Pipeline has a cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    #[error(
        "Step `{step}` needs `{target}`, which declares `workspace: write` and so must be terminal"
    )]
    NeedsWriteStep { step: String, target: String },
    #[error("{referrer} references `{target}`, which declares `workspace: write`")]
    ReferencesWriteStep { referrer: Referrer, target: String },
    #[error(
        "Step `{step}` has a Condition that references `{target}`, which isn't upstream of it; add it to `needs`"
    )]
    NotUpstream { step: String, target: String },
    #[error("Merge Step `{0}` is upstream of the Gate, so it could merge before the Gate decides")]
    MergeBeforeGate(String),
    #[error("Merge Step `{0}` must come after the Gate; add `gate` to its `needs`")]
    MergeNotAfterGate(String),
}

impl LoadError {
    /// Whether this is about what the machine has, a Plugin or Library
    /// Step it lacks, rather than about the file. Such a Pipeline can still
    /// be published, since the developer may be about to install them.
    pub fn is_about_this_machine(&self) -> bool {
        matches!(
            self,
            LoadError::UnknownPlugin { .. }
                | LoadError::UnknownLibraryStep { .. }
                | LoadError::InvalidLibraryStep { .. }
        )
    }
}

/// What holds a reference: the Gate, or one Step's Condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Referrer {
    Gate,
    Condition(String),
}

impl fmt::Display for Referrer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Referrer::Gate => f.write_str("the Gate"),
            Referrer::Condition(step) => write!(f, "the Condition of Step `{step}`"),
        }
    }
}
