//! Workflows as a target: what an edit to a workflow may change, and how a
//! workflow's probe is read. A candidate is a list of step edits. It can
//! change what a workflow asks its agents and how it calls a tool that
//! reads, never which steps there are or what a writing tool is sent.

use std::collections::BTreeSet;

use dispatch::{Receipt, read_trajectory};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workflow::{Engine, Step, Workflow};

use crate::attempt::{Attempt, Base, Candidate, Kind, Loop, MAX_STEPS, Probed, Request, Status};
use crate::guard::{MAX_GROWTH_CHARS, leaks};
use crate::render::Rendered;
use crate::roles::{Brief, WorkflowDraft};

/// The most step runs of one workflow probe kept on an attempt.
const MAX_RUNS: usize = 64;

/// What an attempt improves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
// Inline, so the store can see it's a string it can filter on.
#[schemars(inline)]
pub enum Target {
    /// An agent.
    #[default]
    Agent,
    /// A workflow.
    Workflow,
}

impl Target {
    /// Whether it's an agent, as records that say nothing mean.
    #[must_use]
    pub const fn is_agent(&self) -> bool {
        matches!(self, Self::Agent)
    }

    /// The key a hold on `name` is kept under, so a workflow's hold is
    /// apart from an agent's of the same name.
    #[must_use]
    pub fn key(self, name: &str) -> String {
        match self {
            Self::Agent => name.to_owned(),
            Self::Workflow => format!("workflow:{name}"),
        }
    }

    /// The word for it.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Workflow => "workflow",
        }
    }
}

/// A change to one step of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepEdit {
    /// The step, by its id.
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    /// An agent step's new message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 16_000))]
    pub message: Option<String>,
    /// An agent step's new context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub context: Option<String>,
    /// New arguments for a step whose tool only reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("x-extension" = "checked against the tool's input schema"))]
    pub arguments: Option<Value>,
}

/// The tools whose definitions say they only read.
#[must_use]
pub fn read_tools(engine: &Engine) -> BTreeSet<String> {
    engine
        .registry
        .definitions()
        .into_iter()
        .filter(|definition| definition.effect == dispatch::Effect::Read)
        .map(|definition| definition.name)
        .collect()
}

/// `base` with `edits` made.
///
/// # Errors
///
/// Why an edit can't be made, in a sentence: it names no step, names one
/// twice, or changes what its step may not.
pub fn apply_edits(
    base: &Workflow,
    edits: &[StepEdit],
    reads: &BTreeSet<String>,
) -> Result<Workflow, String> {
    let mut edited = base.clone();
    for (index, edit) in edits.iter().enumerate() {
        let id = &edit.id;
        if edits.iter().take(index).any(|earlier| earlier.id == *id) {
            return Err(format!("step {id} is edited twice"));
        }
        let step = edited
            .steps
            .iter_mut()
            .find(|step| step.id() == id)
            .ok_or_else(|| format!("there is no step {id}"))?;
        match step {
            Step::Agent {
                message, context, ..
            } => {
                if edit.arguments.is_some() {
                    return Err(format!(
                        "step {id} asks an agent; only its message and context can change"
                    ));
                }
                if let Some(text) = &edit.message {
                    text.clone_into(message);
                }
                if let Some(text) = &edit.context {
                    *context = Some(text.clone());
                }
            }
            Step::Tool {
                tool, arguments, ..
            } => {
                if edit.message.is_some() || edit.context.is_some() {
                    return Err(format!(
                        "step {id} calls a tool; only its arguments can change"
                    ));
                }
                if let Some(new) = &edit.arguments {
                    if !reads.contains(tool.as_str()) {
                        return Err(format!(
                            "step {id} calls {tool}, which writes; what it's sent can't change"
                        ));
                    }
                    new.clone_into(arguments);
                }
            }
        }
    }
    Ok(edited)
}

/// The text of each step a judge's edit can reach, by step.
fn texts(workflow: &Workflow) -> Vec<String> {
    workflow
        .steps
        .iter()
        .map(|step| match step {
            Step::Agent {
                message, context, ..
            } => format!("{message}\n{}", context.as_deref().unwrap_or("")),
            Step::Tool { arguments, .. } => arguments.to_string(),
        })
        .collect()
}

/// Checks an edited workflow against the one it would replace: its text
/// may grow by at most [`MAX_GROWTH_CHARS`], and no changed step may hold
/// the task (its inputs) or the judge's input.
///
/// # Errors
///
/// Why the candidate can't be used, in a sentence.
pub fn check_workflow_edit(
    candidate: &Workflow,
    current: &Workflow,
    task: &str,
) -> Result<(), String> {
    let (new, old) = (texts(candidate), texts(current));
    let count = |all: &[String]| all.iter().map(|text| text.chars().count()).sum::<usize>();
    let (grown, was) = (count(&new), count(&old));
    if grown > was.saturating_add(MAX_GROWTH_CHARS) {
        return Err(format!(
            "the steps grew by {} characters; at most {MAX_GROWTH_CHARS} may be added",
            grown - was
        ));
    }
    for (step, (text, before)) in candidate.steps.iter().zip(new.iter().zip(&old)) {
        if text != before {
            leaks(&format!("step {} contains", step.id()), text, task)?;
        }
    }
    Ok(())
}

/// Both candidates' edits: `first`'s, with `second`'s filling in what
/// `first` left alone, field by field.
#[must_use]
pub fn merge_edits(first: &[StepEdit], second: &[StepEdit]) -> Vec<StepEdit> {
    let mut all = first.to_vec();
    for edit in second {
        match all.iter_mut().find(|kept| kept.id == edit.id) {
            Some(kept) => {
                kept.message = kept.message.take().or_else(|| edit.message.clone());
                kept.context = kept.context.take().or_else(|| edit.context.clone());
                kept.arguments = kept.arguments.take().or_else(|| edit.arguments.clone());
            }
            None => all.push(edit.clone()),
        }
    }
    all
}

impl Candidate {
    fn edits(label: &str, kind: Kind, draft: WorkflowDraft) -> Self {
        Self {
            steps: draft.steps,
            ..Self::empty(label, kind, draft.change, draft.cites)
        }
    }
}

/// The tools and agents a workflow's steps use, as the editor sees them.
fn used(workflow: &Workflow, reads: &BTreeSet<String>) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for step in &workflow.steps {
        let name = match step {
            Step::Tool { tool, .. } if reads.contains(tool) => format!("{tool} (reads)"),
            Step::Tool { tool, .. } => format!("{tool} (writes)"),
            Step::Agent { agent, .. } => format!("agent {agent}"),
        };
        if !used.contains(&name) {
            used.push(name);
        }
    }
    used
}

/// Each step's receipt, named by its step, and by its item if it ran once
/// per item.
fn receipts(run: &workflow::WorkflowRun) -> Vec<(String, &Receipt)> {
    let mut all = Vec::new();
    for step in &run.steps {
        if let Some(receipt) = &step.receipt {
            all.push((step.id.clone(), receipt));
        }
        for item in &step.items {
            if let Some(receipt) = &item.receipt {
                all.push((format!("{}[{}]", step.id, item.index), receipt));
            }
        }
    }
    all
}

impl Loop<'_> {
    /// Probes `workflow` on `inputs` and reads its steps' runs back as one,
    /// labelled `label`.
    pub(crate) async fn probe_flow(
        &self,
        name: &str,
        workflow: &Workflow,
        inputs: &Value,
        label: &str,
        attempt: &mut Attempt,
    ) -> Option<(Probed, Rendered)> {
        let probed = match self
            .engine
            .probe_workflow(name, workflow, inputs, self.settings.reads.clone())
            .await
        {
            Ok(probed) => probed,
            Err(why) => {
                attempt.problem(format!("a probe couldn't run: {why}"));
                return None;
            }
        };
        attempt.spend(probed.run.cost_usd_micros);
        let mut parts = Vec::new();
        let mut runs = Vec::new();
        for (id, receipt) in receipts(&probed.run) {
            let trajectory = match receipt.run.parse() {
                Ok(run) => read_trajectory(self.engine.sink.as_ref(), run, MAX_STEPS).await,
                Err(_) => Ok(None),
            };
            let Ok(Some(trajectory)) = trajectory else {
                attempt.problem("a probe's trajectory couldn't be read");
                return None;
            };
            parts.push((id, trajectory));
            runs.push(receipt.run.clone());
        }
        runs.truncate(MAX_RUNS);
        let rendered = Rendered::workflow(label, &parts, &probed.run);
        let summary = Probed {
            run: runs.first().cloned().unwrap_or_default(),
            runs,
            harness: probed.harness,
            outcome: rendered.outcome.clone(),
            calls: rendered.calls.clone(),
        };
        Some((summary, rendered))
    }

    /// Runs one attempt on a workflow: `request.task` is the inputs.
    pub(crate) async fn improve_workflow(&self, request: Request) -> Attempt {
        let mut attempt = Attempt::begun(request.clone());
        let name = &request.agent;
        let workflow = match self.engine.workflow(name).await {
            Ok(Some(workflow)) => workflow,
            Ok(None) => {
                attempt.problem(format!("there is no workflow {name}"));
                return attempt.end(Status::Failed);
            }
            Err(error) => {
                attempt.problem(error.to_string());
                return attempt.end(Status::Failed);
            }
        };
        let inputs = match serde_json::from_str::<Value>(&request.task) {
            Ok(inputs) if inputs.is_object() => inputs,
            _ => {
                attempt.problem("a workflow's task is the inputs to run it on, as a JSON object");
                return attempt.end(Status::Failed);
            }
        };
        attempt.base_harness = Some(self.engine.workflow_harness(&workflow));
        let Some((probed, reference)) = self
            .probe_flow(name, &workflow, &inputs, "R", &mut attempt)
            .await
        else {
            return attempt.end(Status::Failed);
        };
        attempt.reference = Some(probed);
        let lessons = self.lessons(&mut attempt).await;
        let reads = read_tools(self.engine);
        let shown = serde_json::to_string_pretty(&workflow).unwrap_or_default();
        let tools = used(&workflow, &reads);
        let brief = Brief {
            target: Target::Workflow,
            instructions: &shown,
            tools: &tools,
            skills: &[],
            guardrails: &[],
            task: &request.task,
            why: request.why.as_deref(),
            lessons: &lessons,
        };
        let money = self.judge_money(&attempt);
        let edited = match self.judges.edit_workflow(&brief, &reference, money).await {
            Ok(edited) => edited,
            Err(error) => {
                attempt.spend(error.cost_usd_micros());
                attempt.problem(format!("the editor: {error}"));
                return attempt.end(Status::Failed);
            }
        };
        attempt.spend(edited.cost_usd_micros);
        let candidates = [
            Candidate::edits("A", Kind::Invariant, edited.value.invariant),
            Candidate::edits("B", Kind::Domain, edited.value.domain),
        ];
        let base = Base::Workflow {
            workflow,
            inputs,
            reads,
        };
        let runs = self
            .try_all(&mut attempt, &base, candidates, reference)
            .await;
        if runs.len() == 1 {
            attempt.problem("no candidate could be tried");
            return attempt.end(Status::Kept);
        }
        self.decide(attempt, &base, &brief, runs).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use serde_json::json;

    use super::*;

    fn flow() -> Workflow {
        serde_json::from_value(json!({
            "inputs": { "type": "object", "additionalProperties": false,
                        "properties": { "topic": { "type": "string", "maxLength": 100 } } },
            "steps": [
                { "type": "tool", "id": "find", "tool": "look",
                  "arguments": { "query": "{{inputs.topic}}" } },
                { "type": "tool", "id": "keep", "tool": "save",
                  "arguments": { "text": "{{steps.find.result}}" } },
                { "type": "agent", "id": "sum", "agent": "helper", "message": "Sum it up.",
                  "context": "{{steps.find.result}}" }
            ]
        }))
        .unwrap()
    }

    fn reads() -> BTreeSet<String> {
        BTreeSet::from(["look".to_owned()])
    }

    fn edit(value: &Value) -> StepEdit {
        serde_json::from_value(value.clone()).unwrap()
    }

    #[test]
    fn an_edit_changes_what_a_step_asks_and_how_a_read_is_called() {
        let edits = [
            edit(&json!({ "id": "sum", "message": "Sum it up in three lines." })),
            edit(&json!({ "id": "find", "arguments": { "query": "{{inputs.topic}} news" } })),
        ];
        let edited = apply_edits(&flow(), &edits, &reads()).unwrap();
        assert!(
            matches!(&edited.steps[2], Step::Agent { message, .. } if message == "Sum it up in three lines.")
        );
        assert!(matches!(&edited.steps[0], Step::Tool { arguments, .. }
            if arguments["query"] == "{{inputs.topic}} news"));
        assert_eq!(edited.steps[1], flow().steps[1]);
    }

    #[test]
    fn an_edit_may_not_change_a_write_or_the_wrong_part_of_a_step() {
        let refused = |value: Value| apply_edits(&flow(), &[edit(&value)], &reads()).unwrap_err();
        assert!(refused(json!({ "id": "keep", "arguments": { "text": "x" } })).contains("writes"));
        assert!(refused(json!({ "id": "sum", "arguments": {} })).contains("message and context"));
        assert!(refused(json!({ "id": "find", "message": "x" })).contains("only its arguments"));
        assert!(refused(json!({ "id": "nope", "message": "x" })).contains("no step nope"));
        let twice = [
            edit(&json!({ "id": "sum", "message": "a" })),
            edit(&json!({ "id": "sum", "message": "b" })),
        ];
        assert!(
            apply_edits(&flow(), &twice, &reads())
                .unwrap_err()
                .contains("twice")
        );
    }

    #[test]
    fn an_edit_is_held_to_growth_and_leaks() {
        let long = apply_edits(
            &flow(),
            &[edit(
                &json!({ "id": "sum", "message": "x".repeat(MAX_GROWTH_CHARS + 20) }),
            )],
            &reads(),
        )
        .unwrap();
        assert!(
            check_workflow_edit(&long, &flow(), "{}")
                .unwrap_err()
                .contains("grew")
        );
        let task = r#"{"topic":"the quarterly numbers for the board"}"#;
        let pasted = apply_edits(
            &flow(),
            &[edit(
                &json!({ "id": "sum", "message": format!("Sum up {task}") }),
            )],
            &reads(),
        )
        .unwrap();
        assert!(
            check_workflow_edit(&pasted, &flow(), task)
                .unwrap_err()
                .contains("step sum")
        );
        assert_eq!(check_workflow_edit(&flow(), &flow(), task), Ok(()));
    }

    #[test]
    fn a_merge_keeps_the_first_candidates_edit_of_a_field() {
        let first = [edit(&json!({ "id": "sum", "message": "a" }))];
        let second = [
            edit(&json!({ "id": "sum", "message": "b", "context": "c" })),
            edit(&json!({ "id": "find", "arguments": {} })),
        ];
        let merged = merge_edits(&first, &second);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].message.as_deref(), Some("a"));
        assert_eq!(merged[0].context.as_deref(), Some("c"));
        assert_eq!(merged[1].id, "find");
    }
}
