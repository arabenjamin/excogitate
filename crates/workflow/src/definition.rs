//! What a workflow and an agent are, and the checks they pass before running.

use std::collections::{BTreeMap, BTreeSet};

use dispatch::{Budget, Guardrail, check_guardrails};
use plugin_api::ToolDefinition;
use schema::{Validator, bounded, check_strict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::skill::{Skill, check_skills};
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
    /// Its limits. The runtime's default if left out. A workflow's remaining
    /// money and time still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// The longest each of its replies may be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1_000_000))]
    pub max_output_tokens: Option<u32>,
    /// What its memory reads. Left out, every source the plugins offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<AgentMemory>,
    /// Instructions for some tasks, added when the task matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub skills: Vec<Skill>,
    /// Rules on its tools, enforced as it calls them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub guardrails: Vec<Guardrail>,
}

/// What an agent's memory reads, beside the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMemory {
    /// The sources it searches, as `plugin.name`. Empty means none, so a
    /// narrow agent isn't flooded by a large lake.
    #[schemars(length(max = 16), inner(length(min = 1, max = 64)))]
    pub sources: Vec<String>,
}

/// The most items one step may run for.
pub const MAX_ITEMS: u32 = 50;

/// Runs a step once per item of a list, one after another, out of the
/// step's money and time. Each run sees its item as `{{item}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForEach {
    /// One reference to a list, such as `{{steps.find.result.links}}`.
    #[schemars(length(min = 5, max = 256))]
    pub items: String,
    /// The most items it runs for. A longer list is cut, and the step's
    /// `fan_out` says so.
    #[schemars(range(min = 1, max = MAX_ITEMS))]
    pub max_items: u32,
    /// Whether an item that fails lets the rest run. If not, the first
    /// failure fails the step.
    #[serde(default)]
    pub continue_on_failure: bool,
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
        /// Runs it once per item, which the arguments see as `{{item}}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_each: Option<ForEach>,
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
        /// Asks it once per item, which only `context` may use, as `{{item}}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_each: Option<ForEach>,
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

    /// The list it runs over, if it runs once per item.
    #[must_use]
    pub const fn for_each(&self) -> Option<&ForEach> {
        match self {
            Self::Tool { for_each, .. } | Self::Agent { for_each, .. } => for_each.as_ref(),
        }
    }

    /// The field of its output that later steps read: `result` for a tool,
    /// `answer` for an agent.
    #[must_use]
    pub const fn output_field(&self) -> &'static str {
        match self {
            Self::Tool { .. } => "result",
            Self::Agent { .. } => "answer",
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
    if let Some(missing) = agent.tools.iter().find(|name| !tools.contains_key(*name)) {
        return Err(format!("there is no tool {missing}"));
    }
    check_skills(&agent.skills)?;
    check_guardrails(&agent.guardrails, &agent.tools)
}

/// Checks that the sources an agent's memory names exist.
///
/// # Errors
///
/// What's wrong, in a sentence.
pub fn check_agent_sources(agent: &Agent, sources: &[String]) -> Result<(), String> {
    let named = agent.memory.iter().flat_map(|memory| &memory.sources);
    match named.clone().find(|name| !sources.contains(*name)) {
        Some(missing) => Err(format!(
            "its memory names the source {missing}, which no plugin offers"
        )),
        None if named.clone().count() > 0 && !agent.tools.iter().any(|tool| tool == "memory") => {
            Err("its memory names sources, and it doesn't have the memory tool".to_owned())
        }
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
    /// True inside a step that runs once per item, which may use `item`.
    item: bool,
}

impl Scope<'_> {
    fn check_reference(&self, path: &str) -> Result<(), String> {
        let mut segments = path.split('.');
        match (segments.next(), segments.next(), segments.next()) {
            (Some("inputs"), Some(name), _) if self.inputs.contains(name) => Ok(()),
            (Some("inputs"), name, _) => Err(format!("there is no input {}", name.unwrap_or(""))),
            (Some("steps"), Some(id), field) => self.check_step_reference(path, id, field),
            (Some("item"), ..) if self.item => Ok(()),
            (Some("item"), ..) => Err(format!("{path}: only a step with for_each has an item")),
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
        let allowed = step.output_field();
        if field == Some(allowed) || (step.for_each().is_some() && field == Some("fan_out")) {
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

/// A step's list must be one reference, to the inputs or an earlier step.
fn check_items(each: &ForEach, scope: &Scope<'_>) -> Result<(), String> {
    if !(1..=MAX_ITEMS).contains(&each.max_items) {
        return Err(format!("for_each.max_items must be 1 to {MAX_ITEMS}"));
    }
    let items = each.items.as_str();
    let found = references(items).map_err(|error| error.to_string())?;
    let [path] = found.as_slice() else {
        return Err("for_each.items must be one reference to a list".to_owned());
    };
    if items.trim() != format!("{{{{{path}}}}}") {
        return Err("for_each.items must be one reference to a list, and nothing else".to_owned());
    }
    scope
        .check_reference(path)
        .map_err(|problem| format!("for_each.items: {problem}"))
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
            let uses_item = references(message)
                .map_err(|error| error.to_string())?
                .into_iter()
                .any(|path| path == "item" || path.starts_with("item."));
            if uses_item {
                return Err("a message can't use the item: put that in context".to_owned());
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
        item: false,
    };
    for step in &workflow.steps {
        let id = step.id();
        if scope.earlier.contains_key(id) {
            return Err(format!("two steps are named {id}"));
        }
        if let Some(each) = step.for_each() {
            check_items(each, &scope).map_err(|problem| format!("step {id}: {problem}"))?;
        }
        scope.item = step.for_each().is_some();
        let checked = check_step(step, &scope, tools, agents);
        scope.item = false;
        checked.map_err(|problem| format!("step {id}: {problem}"))?;
        scope.earlier.insert(id, step);
    }
    workflow
        .output
        .as_ref()
        .map_or(Ok(()), |output| scope.check_text(output))
        .map_err(|problem| format!("its output: {problem}"))
}
