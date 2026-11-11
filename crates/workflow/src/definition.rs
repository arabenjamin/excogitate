//! What a workflow and an agent are, and the checks they pass before running.

use std::collections::{BTreeMap, BTreeSet};

use dispatch::Budget;
use plugin_api::ToolDefinition;
use schema::{Validator, bounded, check_strict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::template::references;

/// A sub-agent: a chat with its own instructions and tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// What it's for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[schemars(length(max = 1024))]
    pub description: String,
    /// Its instructions.
    #[schemars(length(min = 1, max = 16_000))]
    pub system_prompt: String,
    /// The tools it may call. None is fine.
    #[serde(default)]
    #[schemars(length(max = 16), inner(length(max = 64)))]
    pub tools: Vec<String>,
    /// Its limits. The brain's default if left out. A workflow's remaining
    /// money and time still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// The longest each of its replies may be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1_000_000))]
    pub max_output_tokens: Option<u32>,
}

/// One step of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Calls a tool.
    Tool {
        /// Its name, which later steps refer to.
        #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
        id: String,
        /// The tool.
        #[schemars(length(min = 1, max = 64))]
        tool: String,
        /// Its arguments. Strings may hold references.
        #[schemars(extend("x-extension" = "checked against the tool's input schema"))]
        arguments: Value,
    },
    /// Asks a sub-agent.
    Agent {
        /// Its name, which later steps refer to.
        #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
        id: String,
        /// The agent.
        #[schemars(length(min = 1, max = 64))]
        agent: String,
        /// What to ask. It may refer to `inputs`, but not to steps: what
        /// earlier steps found goes in `context`, labelled as data.
        #[schemars(length(min = 1, max = 16_000))]
        message: String,
        /// What it should work from, usually an earlier step's output.
        #[schemars(length(max = 4096))]
        context: Option<String>,
    },
}

impl Step {
    /// The step's name.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Tool { id, .. } | Self::Agent { id, .. } => id,
        }
    }
}

/// A fixed list of steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// What it's for.
    #[serde(default)]
    #[schemars(length(max = 1024))]
    pub description: String,
    /// The JSON Schema of its inputs, which must be strict.
    #[schemars(extend("x-extension" = "the workflow's own schema, checked for strictness"))]
    pub inputs: Value,
    /// The steps, in order.
    #[schemars(length(min = 1, max = 32))]
    pub steps: Vec<Step>,
    /// What it returns: a reference, or text with references in it.
    #[schemars(length(max = 4096))]
    pub output: Option<String>,
    /// The most the whole run may spend, in millionths of a dollar.
    #[serde(default)]
    pub max_usd_micros: u64,
}

/// Checks an agent against the tools that exist.
///
/// # Errors
///
/// What's wrong, in a sentence.
pub fn check_agent(agent: &Agent, tools: &BTreeMap<String, ToolDefinition>) -> Result<(), String> {
    match agent.tools.iter().find(|name| !tools.contains_key(*name)) {
        Some(missing) => Err(format!("there is no tool {missing}")),
        None => Ok(()),
    }
}

fn check_inputs_schema(inputs: &Value) -> Result<(), String> {
    if let Some(violation) = check_strict(inputs).first() {
        return Err(format!("its inputs schema isn't strict: {violation}"));
    }
    Validator::from_schema(inputs)
        .map(|_| ())
        .map_err(|error| bounded(&error.to_string()))
}

fn input_names(inputs: &Value) -> BTreeSet<&str> {
    inputs
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// What's been seen so far as the steps are walked.
struct Scope<'a> {
    inputs: BTreeSet<&'a str>,
    earlier: BTreeMap<&'a str, &'a Step>,
}

impl Scope<'_> {
    fn check_reference(&self, path: &str) -> Result<(), String> {
        let mut segments = path.split('.');
        match (segments.next(), segments.next(), segments.next()) {
            (Some("inputs"), Some(name), _) if self.inputs.contains(name) => Ok(()),
            (Some("inputs"), name, _) => Err(format!("there is no input {}", name.unwrap_or(""))),
            (Some("steps"), Some(id), field) => self.check_step_reference(path, id, field),
            _ => Err(format!("{path} must start with inputs. or steps.")),
        }
    }

    fn check_step_reference(
        &self,
        path: &str,
        id: &str,
        field: Option<&str>,
    ) -> Result<(), String> {
        let Some(step) = self.earlier.get(id) else {
            return Err(format!("{path} refers to a step that hasn't run yet"));
        };
        let allowed = match step {
            Step::Tool { .. } => "result",
            Step::Agent { .. } => "answer",
        };
        if field == Some(allowed) {
            Ok(())
        } else {
            Err(format!("{path}: that step's output is under .{allowed}"))
        }
    }

    fn check_text(&self, text: &str) -> Result<(), String> {
        for path in references(text).map_err(|error| error.to_string())? {
            self.check_reference(path)?;
        }
        Ok(())
    }

    fn check_value(&self, value: &Value) -> Result<(), String> {
        match value {
            Value::String(text) => self.check_text(text),
            Value::Array(items) => items.iter().try_for_each(|item| self.check_value(item)),
            Value::Object(fields) => fields
                .values()
                .try_for_each(|field| self.check_value(field)),
            _ => Ok(()),
        }
    }
}

/// The value at a JSON pointer, if it's a string holding a reference.
fn is_template_at(value: &Value, pointer: &str) -> bool {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains("{{"))
}

/// Checks a step's arguments against the tool. A value that's filled in when
/// the step runs can't be judged yet, and is checked again then.
fn check_arguments(definition: &ToolDefinition, arguments: &Value) -> Result<(), String> {
    let validator =
        Validator::from_schema(&definition.input_schema).map_err(|error| error.to_string())?;
    let Err(refusal) = validator.check(arguments) else {
        return Ok(());
    };
    let real: Vec<String> = refusal
        .problems
        .iter()
        .filter(|problem| !is_template_at(arguments, &problem.path))
        .map(|problem| format!("{}: {}", problem.path, problem.message))
        .collect();
    if real.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "its arguments for {}: {}",
            definition.name,
            real.join("; ")
        ))
    }
}

fn check_step(
    step: &Step,
    scope: &Scope<'_>,
    tools: &BTreeMap<String, ToolDefinition>,
    agents: &BTreeSet<String>,
) -> Result<(), String> {
    match step {
        Step::Tool {
            tool, arguments, ..
        } => {
            let definition = tools
                .get(tool)
                .ok_or_else(|| format!("there is no tool {tool}"))?;
            scope.check_value(arguments)?;
            check_arguments(definition, arguments)
        }
        Step::Agent {
            agent,
            message,
            context,
            ..
        } => {
            if !agents.contains(agent) {
                return Err(format!("there is no agent {agent}"));
            }
            scope.check_text(message)?;
            let from_a_step = references(message)
                .map_err(|error| error.to_string())?
                .into_iter()
                .any(|path| path.starts_with("steps."));
            if from_a_step {
                return Err("a message can't use a step's output: put that in context".to_owned());
            }
            context
                .as_ref()
                .map_or(Ok(()), |text| scope.check_text(text))
        }
    }
}

/// Checks a workflow against the tools and agents that exist. On an error,
/// nothing about it can run.
///
/// # Errors
///
/// What's wrong, naming the step.
pub fn check_workflow(
    workflow: &Workflow,
    tools: &BTreeMap<String, ToolDefinition>,
    agents: &BTreeSet<String>,
) -> Result<(), String> {
    check_inputs_schema(&workflow.inputs)?;
    let mut scope = Scope {
        inputs: input_names(&workflow.inputs),
        earlier: BTreeMap::new(),
    };
    for step in &workflow.steps {
        let id = step.id();
        if scope.earlier.contains_key(id) {
            return Err(format!("two steps are named {id}"));
        }
        check_step(step, &scope, tools, agents)
            .map_err(|problem| format!("step {id}: {problem}"))?;
        scope.earlier.insert(id, step);
    }
    workflow
        .output
        .as_ref()
        .map_or(Ok(()), |output| scope.check_text(output))
        .map_err(|problem| format!("its output: {problem}"))
}
