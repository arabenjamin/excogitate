//! Probes: an agent run under a candidate harness, with no side effects, so
//! Harness² can see how the candidate behaves before anything changes.

use std::collections::{BTreeMap, BTreeSet};

use dispatch::{Probe, Receipt};
use schema::{Validator, bounded};
use schemars::JsonSchema;
use serde::Serialize;

use crate::engine::{Engine, Left};
use crate::{Agent, check_agent};

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
        check_agent(candidate, &tools)?;
        let own = candidate.budget.unwrap_or(self.limits.agent_budget);
        let left = Left {
            money: own.max_usd_micros,
            time: self.limits.time_limit,
        };
        let result = self
            .converse_as(
                &format!("probe:{name}"),
                candidate,
                task.to_owned(),
                left,
                Some(Probe { reads }),
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
