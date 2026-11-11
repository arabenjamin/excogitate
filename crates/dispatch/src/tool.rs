//! The registry that checks tools. The tool types themselves are part of the
//! plugin contract, in `plugin-api`.

use std::collections::BTreeMap;
use std::sync::Arc;

use plugin_api::{Tool, ToolDefinition};
use schema::{InvalidSchema, Validator, check_strict};
use serde_json::Value;

/// The longest tool name, which is also what the wire format allows.
const MAX_NAME_CHARS: usize = 64;
/// The longest tool description.
const MAX_DESCRIPTION_CHARS: usize = 2048;

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

/// Every tool the runtime has. What a run may call is a subset of these.
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

    /// What each registered tool declares, by name.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|registered| registered.definition.clone())
            .collect()
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
