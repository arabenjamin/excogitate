//! Tools, and the compiled-in form of a plugin.

use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DataClass, RunId, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Capabilities, CapabilityError, PluginCapability};

/// What a tool can do to the world. A write counts against the write budget
/// and is stubbed when Harness² probes a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Effect {
    /// Only reads.
    Read,
    /// Changes something.
    Write,
}

/// What a tool declares about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolDefinition {
    /// Its name.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// The strict JSON Schema of its arguments.
    pub input_schema: Value,
    /// The strict JSON Schema of its result.
    pub output_schema: Value,
    /// What it can do to the world.
    pub effect: Effect,
}

impl ToolDefinition {
    /// A definition with schemas written out.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        output_schema: Value,
        effect: Effect,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema,
            effect,
        }
    }

    /// A definition whose schemas come from Rust types.
    #[must_use]
    pub fn for_types<I: JsonSchema, O: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        effect: Effect,
    ) -> Self {
        Self::new(
            name,
            description,
            schema::schema_of::<I>(),
            schema::schema_of::<O>(),
            effect,
        )
    }
}

/// What the runtime tells a tool about the call. None of it comes from the
/// model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolContext {
    /// The run making the call.
    pub run: RunId,
    /// The privacy class of the run's data.
    pub data_class: DataClass,
    /// What's left of the run's money budget, in millionths of a dollar. A
    /// tool that spends passes this on, so it can't overrun the run.
    pub budget_usd_micros: u64,
    /// How many more tool calls the run may make. A plugin's capability
    /// calls count as tool calls, so past this they're refused.
    pub tool_calls_left: u32,
    /// How long the run has left, in milliseconds. The call is stopped at
    /// this, so work it starts, such as another agent's run, must end
    /// sooner.
    pub time_left_ms: u64,
    /// True in a Harness² probe: a tool that runs at all must change
    /// nothing, and work it starts must be a probe too.
    pub probing: bool,
    /// In a plugin process, the host's token for this one tool call, which
    /// its capability calls carry back.
    pub(crate) call: Option<uuid::Uuid>,
}

impl ToolContext {
    /// The context for a call made by `run`.
    #[must_use]
    pub const fn new(run: RunId, data_class: DataClass, budget_usd_micros: u64) -> Self {
        Self {
            run,
            data_class,
            budget_usd_micros,
            tool_calls_left: u32::MAX,
            time_left_ms: u64::MAX,
            probing: false,
            call: None,
        }
    }

    /// The same context, marked as a probe or not.
    #[must_use]
    pub const fn with_probing(mut self, probing: bool) -> Self {
        self.probing = probing;
        self
    }

    /// The same context, with how long the run has left.
    #[must_use]
    pub const fn with_time_left_ms(mut self, time_left_ms: u64) -> Self {
        self.time_left_ms = time_left_ms;
        self
    }

    /// The same context, with how many more tool calls the run may make.
    #[must_use]
    pub const fn with_tool_calls_left(mut self, tool_calls_left: u32) -> Self {
        self.tool_calls_left = tool_calls_left;
        self
    }
}

/// One capability call a plugin's tool made, as the host recorded it. It
/// goes on the run's trajectory under that tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapabilityUse {
    /// Which capability.
    pub capability: PluginCapability,
    /// What it was for: the method and address, or the model that answered.
    pub what: String,
    /// Whether it worked.
    pub ok: bool,
    /// What it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// How long it took.
    pub duration_ms: u64,
}

impl CapabilityUse {
    /// A use of `capability` for `what`, free and instant until said
    /// otherwise.
    #[must_use]
    pub fn new(capability: PluginCapability, what: impl Into<String>, ok: bool) -> Self {
        Self {
            capability,
            what: what.into(),
            ok,
            cost_usd_micros: 0,
            duration_ms: 0,
        }
    }

    /// The same use, with what it cost.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }

    /// The same use, with how long it took.
    #[must_use]
    pub const fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }
}

/// What a tool returned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolOutput {
    /// The result, which must meet the output schema.
    pub value: Value,
    /// Whether it may be treated as instructions. Fetched content is
    /// untrusted, and the label goes to the model with it.
    pub trust: Trust,
    /// What the call spent, in millionths of a dollar. It goes on the run's
    /// receipt and against its budget.
    pub cost_usd_micros: u64,
    /// The capability calls it made. The host sets these for a plugin's
    /// tools, whatever the plugin says.
    pub uses: Vec<CapabilityUse>,
}

impl ToolOutput {
    /// A result that cost nothing.
    #[must_use]
    pub fn new(value: Value, trust: Trust) -> Self {
        Self {
            value,
            trust,
            cost_usd_micros: 0,
            uses: Vec::new(),
        }
    }

    /// The same result, with what the call spent.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }
}

/// A tool could not do its job. Different from a poor result, which is still
/// a successful call. What it spent before failing still goes on the run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ToolFailure {
    message: String,
    cost_usd_micros: u64,
    uses: Vec<CapabilityUse>,
}

impl ToolFailure {
    /// A failure, saying why.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cost_usd_micros: 0,
            uses: Vec::new(),
        }
    }

    /// The same failure, with the capability calls made before it.
    #[must_use]
    pub fn with_uses(mut self, uses: Vec<CapabilityUse>) -> Self {
        self.uses = uses;
        self
    }

    /// The capability calls made before it failed.
    #[must_use]
    pub fn uses(&self) -> &[CapabilityUse] {
        &self.uses
    }

    /// The same failure, with what the call spent before it failed.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }

    /// Why it failed.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// What the call spent before it failed, in millionths of a dollar.
    #[must_use]
    pub const fn cost_usd_micros(&self) -> u64 {
        self.cost_usd_micros
    }
}

impl From<String> for ToolFailure {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ToolFailure {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

impl From<CapabilityError> for ToolFailure {
    fn from(error: CapabilityError) -> Self {
        Self::new(error.to_string())
    }
}

/// Something a model can call.
#[async_trait]
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// What it declares about itself.
    fn definition(&self) -> ToolDefinition;

    /// What this call can do to the world, once its arguments have met the
    /// schema. Override it when one tool has both reads and writes; a read
    /// must never be reported for something that changes anything.
    fn effect_of(&self, _arguments: &Value) -> Effect {
        self.definition().effect
    }

    /// Does the work. `arguments` has already met the input schema.
    ///
    /// # Errors
    ///
    /// [`ToolFailure`] if it couldn't.
    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure>;
}

/// A plugin could not set itself up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// It needs a capability it wasn't given.
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// A plugin compiled into the binary.
pub trait Plugin: Send + Sync + std::fmt::Debug {
    /// The text of its manifest, usually `include_str!("../plugin.yaml")`.
    fn manifest(&self) -> &str;

    /// Builds its tools, using only the capabilities it's handed. They must
    /// be exactly the tools its manifest declares.
    ///
    /// # Errors
    ///
    /// [`PluginError`] if it can't be set up.
    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError>;
}
