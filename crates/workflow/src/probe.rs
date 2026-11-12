//! Probes: an agent or a workflow run under a candidate harness, with no
//! side effects, so Harness² can see how the candidate behaves before
//! anything changes.

use std::collections::{BTreeMap, BTreeSet};

use dispatch::{Probe, Receipt, harness_id};
use schema::{Validator, bounded};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use store::{DefinitionKind, MAX_PAGE_LIMIT, PageRequest};

use crate::engine::{Engine, Left};
use crate::{
    Agent, DELEGATE_TOOL, Workflow, WorkflowError, WorkflowRun, check_agent, check_workflow,
};

/// How a candidate workflow behaved on its inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowProbe {
    /// The candidate's harness id.
    pub harness: String,
    /// The run. Each step's receipt names its run, whose trajectory has
    /// every step.
    pub run: WorkflowRun,
}

/// The most agents read to check a candidate workflow's steps against.
const MAX_AGENTS: usize = 10_000;

/// How a candidate behaved on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProbeRun {
    /// The run, whose trajectory has every step.
    #[schemars(length(max = 36))]
    pub run: String,
    /// The candidate's harness id.
    #[schemars(length(max = 64))]
    pub harness: String,
    /// What it answered, if it did.
    #[schemars(length(max = 1_000_000))]
    pub answer: Option<String>,
    /// Why it didn't.
    #[schemars(length(max = 2048))]
    pub stopped_because: Option<String>,
    /// What actually ran, with the stubbed calls marked `stubbed`.
    pub receipt: Receipt,
}

impl Engine {
    /// The workflow defined as `name`, if there is one.
    ///
    /// # Errors
    ///
    /// [`WorkflowError`] if the store fails or the definition doesn't parse.
    pub async fn workflow(&self, name: &str) -> Result<Option<Workflow>, WorkflowError> {
        self.definition(DefinitionKind::Workflow, name).await
    }

    /// What decides how `workflow` behaves, for its harness id: its
    /// definition. Its agents have harness ids of their own.
    #[must_use]
    pub fn workflow_harness(&self, workflow: &Workflow) -> String {
        harness_id(&json!({ "workflow": workflow }))
    }

    /// The names of the agents defined now.
    async fn agent_names(&self) -> Result<BTreeSet<String>, String> {
        let mut names = BTreeSet::new();
        let mut offset = 0;
        loop {
            let page = PageRequest::new(MAX_PAGE_LIMIT, offset).map_err(|e| e.to_string())?;
            let listed = self
                .store
                .list_definitions(DefinitionKind::Agent, page)
                .await
                .map_err(|error| bounded(&error.to_string()))?;
            names.extend(listed.items.into_iter().map(|found| found.key.name));
            if !listed.has_more || names.len() >= MAX_AGENTS {
                return Ok(names);
            }
            offset = offset.saturating_add(MAX_PAGE_LIMIT);
        }
    }

    /// Checks a workflow as it would be checked to load: its schema, and
    /// its steps against the tools and agents there are.
    ///
    /// # Errors
    ///
    /// What's wrong with it, in a sentence.
    pub async fn check_workflow_candidate(&self, candidate: &Workflow) -> Result<(), String> {
        let raw = serde_json::to_value(candidate).map_err(|error| bounded(&error.to_string()))?;
        Validator::for_type::<Workflow>()
            .map_err(|error| bounded(&error.to_string()))?
            .check(&raw)
            .map_err(|refusal| bounded(&format!("the candidate: {refusal}")))?;
        let tools: BTreeMap<_, _> = self
            .registry
            .definitions()
            .into_iter()
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        check_workflow(candidate, &tools, &self.agent_names().await?)
    }

    /// Runs `candidate` as the workflow `name` on `inputs`, every step as a
    /// probe: writes never run, reads run only if they're in `reads`, and
    /// agent steps run as probes of their agents. A stubbed tool step's
    /// output is the stub.
    ///
    /// # Errors
    ///
    /// What's wrong, in a sentence: the candidate fails its checks, or the
    /// inputs don't fit it.
    pub async fn probe_workflow(
        &self,
        name: &str,
        candidate: &Workflow,
        inputs: &Value,
        reads: BTreeSet<String>,
    ) -> Result<WorkflowProbe, String> {
        self.check_workflow_candidate(candidate).await?;
        Validator::from_schema(&candidate.inputs)
            .map_err(|error| bounded(&error.to_string()))?
            .check(inputs)
            .map_err(|refusal| bounded(&format!("the inputs: {refusal}")))?;
        let probe = Probe {
            reads,
            nested: BTreeSet::from([DELEGATE_TOOL.to_owned()]),
        };
        let run = self
            .run_steps(name, candidate, inputs.clone(), Some(&probe))
            .await;
        Ok(WorkflowProbe {
            harness: self.workflow_harness(candidate),
            run,
        })
    }

    /// Checks an agent as it would be checked to run: its schema, its
    /// tools, its skills and its guardrails.
    ///
    /// # Errors
    ///
    /// What's wrong with it, in a sentence.
    pub fn check_candidate(&self, candidate: &Agent) -> Result<(), String> {
        let raw = serde_json::to_value(candidate).map_err(|error| bounded(&error.to_string()))?;
        Validator::for_type::<Agent>()
            .map_err(|error| bounded(&error.to_string()))?
            .check(&raw)
            .map_err(|refusal| bounded(&format!("the candidate: {refusal}")))?;
        let tools: BTreeMap<_, _> = self
            .registry
            .definitions()
            .into_iter()
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        check_agent(candidate, &tools)
    }

    /// Runs `candidate` as the agent `name` on `task`. Every call is checked as
    /// usual. Writes never run, and reads run only if they're in `reads`. The
    /// trajectory is recorded as `probe:<name>`, apart from the agent's real
    /// runs.
    ///
    /// # Errors
    ///
    /// What's wrong with the candidate, in a sentence: it fails the agent
    /// schema, or names a tool that doesn't exist.
    pub async fn probe_agent(
        &self,
        name: &str,
        candidate: &Agent,
        task: &str,
        reads: BTreeSet<String>,
    ) -> Result<ProbeRun, String> {
        self.check_candidate(candidate)?;
        let own = candidate.budget.unwrap_or(self.limits.agent_budget);
        let left = Left {
            money: own.max_usd_micros,
            time: self.limits.time_limit,
        };
        let result = self
            .converse_as(
                name,
                &format!("probe:{name}"),
                candidate,
                task.to_owned(),
                left,
                Some(Probe {
                    reads,
                    // A hand-off runs, as a probe of the agent it goes to.
                    nested: BTreeSet::from([DELEGATE_TOOL.to_owned()]),
                }),
            )
            .await?;
        Ok(ProbeRun {
            run: result.receipt.run.clone(),
            harness: self.agent_harness(candidate),
            answer: result.answer,
            stopped_because: result.stopped_because,
            receipt: result.receipt,
        })
    }
}
