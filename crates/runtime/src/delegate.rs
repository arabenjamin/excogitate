//! `delegate`: hands a task to another agent, which runs as its own run with
//! its own tools and budget, and returns its answer. It's how a front door
//! with a few tools of its own reaches everything else.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use dispatch::{Outcome, ToolRegistry};
use events::EventSink;
use model::Gateway;
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::Trust;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::Store;
use workflow::{DELEGATE_TOOL, Engine, EngineLimits};

const DESCRIPTION: &str = "Hands a task to one of the agents listed in your instructions, and \
returns its answer. Say everything it needs in `task`: it sees nothing of this conversation. \
Put material it should work from, such as text the user gave you, in `context`.";

/// What the model sends.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    /// The agent, by name.
    #[schemars(length(min = 1, max = 64))]
    agent: String,
    /// What it should do, in full.
    #[schemars(length(min = 1, max = 16_000))]
    task: String,
    /// What it should work from. It's handed over as data, not instructions.
    #[schemars(length(max = 16_000))]
    context: Option<String>,
}

/// What comes back.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct DelegateResult {
    /// The agent that ran.
    #[schemars(length(max = 64))]
    agent: String,
    /// Its answer, if it gave one.
    #[schemars(length(max = 200_000))]
    answer: Option<String>,
    /// How its run ended.
    #[schemars(length(max = 32))]
    outcome: String,
    /// Why it stopped without answering, if it did.
    #[schemars(length(max = 2048))]
    stopped_because: Option<String>,
    /// Its run, to read in full at `/api/v1/runs/{run}`.
    #[schemars(length(max = 64))]
    run: String,
}

/// The tool. It holds the registry it sits in weakly, so the two don't keep
/// each other, and the store, alive.
#[derive(Debug)]
pub(crate) struct DelegateTool {
    pub(crate) registry: Weak<ToolRegistry>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) sink: Arc<dyn EventSink>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) limits: EngineLimits,
}

impl DelegateTool {
    fn engine(&self) -> Option<Engine> {
        Some(Engine {
            registry: self.registry.upgrade()?,
            gateway: Arc::clone(&self.gateway),
            sink: Arc::clone(&self.sink),
            store: Arc::clone(&self.store),
            limits: self.limits,
        })
    }
}

fn outcome_name(outcome: Outcome) -> String {
    serde_json::to_value(outcome)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{outcome:?}"))
}

#[async_trait]
impl Tool for DelegateTool {
    fn definition(&self) -> ToolDefinition {
        // A hand-off can lead to writes, so it counts as one, and a probe
        // stubs it rather than run another agent for real.
        ToolDefinition::for_types::<DelegateArgs, DelegateResult>(
            DELEGATE_TOOL,
            DESCRIPTION,
            Effect::Write,
        )
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: DelegateArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure(schema::bounded(&error.to_string())))?;
        let engine = self
            .engine()
            .ok_or_else(|| ToolFailure("the runtime is shutting down".to_owned()))?;
        let result = engine
            .delegate(
                &args.agent,
                &args.task,
                args.context.as_deref(),
                context.budget_usd_micros,
            )
            .await
            .map_err(ToolFailure)?;
        let cost_usd_micros = result.receipt.totals.cost_usd_micros;
        let reply = DelegateResult {
            agent: args.agent,
            outcome: outcome_name(result.receipt.outcome),
            run: result.receipt.run.clone(),
            answer: result.answer,
            stopped_because: result.stopped_because,
        };
        Ok(ToolOutput {
            value: serde_json::to_value(reply)
                .map_err(|error| ToolFailure(schema::bounded(&error.to_string())))?,
            // A model wrote it, after reading whatever its tools brought in.
            trust: Trust::Untrusted,
            cost_usd_micros,
        })
    }
}
