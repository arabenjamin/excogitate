//! Tools, and the compiled-in form of a plugin.

use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DataClass, RunId, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Capabilities, CapabilityError};

/// What a tool can do to the world. A write counts against the write budget
/// and is stubbed when Harness² probes a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Only reads.
    Read,
    /// Changes something.
    Write,
}

/// What a tool declares about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A definition whose schemas come from Rust types.
    #[must_use]
    pub fn for_types<I: JsonSchema, O: JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        effect: Effect,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: schema::schema_of::<I>(),
            output_schema: schema::schema_of::<O>(),
            effect,
        }
    }
}

/// What the brain tells a tool about the call. None of it comes from the
/// model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolContext {
    /// The run making the call.
    pub run: RunId,
    /// The privacy class of the run's data.
    pub data_class: DataClass,
}

/// What a tool returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// The result, which must meet the output schema.
    pub value: Value,
    /// Whether it may be treated as instructions. Fetched content is
    /// untrusted, and the label goes to the model with it.
    pub trust: Trust,
}

/// A tool could not do its job. Different from a poor result, which is still
/// a successful call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolFailure(pub String);

/// Something a model can call.
#[async_trait]
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// What it declares about itself.
    fn definition(&self) -> ToolDefinition;

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
