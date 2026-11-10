//! Tools, and the registry that checks them.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DataClass, RunId, Trust};
use schema::{InvalidSchema, Validator, check_strict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The longest tool name, which is also what the wire format allows.
const MAX_NAME_CHARS: usize = 64;
/// The longest tool description.
const MAX_DESCRIPTION_CHARS: usize = 2048;

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

/// A tool could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// Two tools claimed one name. Exactly one tool owns a name.
    #[error("tool {0} is already registered")]
    Duplicate(String),
    /// The definition doesn't meet the rules.
    #[error("tool {name}: {problem}")]
    Invalid {
        /// The tool.
        name: String,
        /// What's wrong.
        problem: String,
    },
}

#[derive(Debug)]
pub(crate) struct Registered {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) definition: ToolDefinition,
    pub(crate) input: Validator,
    pub(crate) output: Validator,
}

/// Every tool the brain has. What a run may call is a subset of these.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Registered>,
}

fn name_is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= MAX_NAME_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool, after checking what it declares.
    ///
    /// # Errors
    ///
    /// [`RegistryError`] if the name is taken or badly formed, the
    /// description is too long, or a schema isn't strict.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let definition = tool.definition();
        let name = definition.name.clone();
        let problem = |problem: String| RegistryError::Invalid {
            name: name.clone(),
            problem,
        };
        if !name_is_valid(&name) {
            return Err(problem(
                "the name must match ^[a-z][a-z0-9_]{0,63}$".to_owned(),
            ));
        }
        if definition.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(problem("the description is too long".to_owned()));
        }
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        let compile = |label: &str, schema: &Value| -> Result<Validator, RegistryError> {
            if let Some(violation) = check_strict(schema).first() {
                return Err(problem(format!(
                    "{label} schema isn't strict at {violation}"
                )));
            }
            Validator::from_schema(schema)
                .map_err(|error: InvalidSchema| problem(format!("{label} schema: {error}")))
        };
        let input = compile("input", &definition.input_schema)?;
        let output = compile("output", &definition.output_schema)?;
        self.tools.insert(
            name.clone(),
            Registered {
                tool,
                definition,
                input,
                output,
            },
        );
        Ok(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Registered> {
        self.tools.get(name)
    }
}
