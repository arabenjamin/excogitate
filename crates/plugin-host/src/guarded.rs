//! The `http` and `model` capabilities, as the host hands them out. The
//! runtime makes them; the host makes sure what they let in is labelled and
//! paid for, whatever the plugin says.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use plugin_api::{
    HttpCapability, ModelCapability, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput,
};
use protocol::{RunId, Trust};
use serde_json::Value;

/// What a plugin's model calls have cost, per run, not yet charged.
#[derive(Debug, Default)]
pub struct SpendMeter {
    spent: Mutex<HashMap<RunId, u64>>,
}

impl SpendMeter {
    /// Adds a call's cost to its run.
    pub fn add(&self, run: RunId, cost_usd_micros: u64) {
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        let total = spent.entry(run).or_default();
        *total = total.saturating_add(cost_usd_micros);
    }

    /// What the run has spent and not yet been charged.
    #[must_use]
    pub fn pending(&self, run: RunId) -> u64 {
        self.spent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&run)
            .copied()
            .unwrap_or(0)
    }

    /// Takes what the run has spent, to charge it.
    fn take(&self, run: RunId) -> u64 {
        self.spent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&run)
            .unwrap_or(0)
    }
}

/// Where the `http` and `model` capabilities come from. The runtime gives
/// the host one; without it, a plugin asking for either is refused.
pub trait CapabilitySource: Send + Sync + std::fmt::Debug {
    /// HTTP for the plugin named.
    fn http(&self, plugin: &str) -> Arc<dyn HttpCapability>;

    /// Model calls for the plugin named. Each call adds its cost to `meter`,
    /// and may spend only what the run has left after what's pending there.
    fn model(&self, plugin: &str, meter: Arc<SpendMeter>) -> Arc<dyn ModelCapability>;
}

/// A tool of a plugin that holds `http` or `model`. What it returns came
/// from outside, or from a model, so it's untrusted; and what its model calls
/// cost goes on the run, even if the plugin reports less.
#[derive(Debug)]
pub(crate) struct GuardedTool {
    pub(crate) inner: Arc<dyn Tool>,
    pub(crate) meter: Option<Arc<SpendMeter>>,
}

#[async_trait]
impl Tool for GuardedTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn effect_of(&self, arguments: &Value) -> plugin_api::Effect {
        self.inner.effect_of(arguments)
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let outcome = self.inner.run(arguments, context).await;
        let metered = self
            .meter
            .as_ref()
            .map_or(0, |meter| meter.take(context.run));
        outcome.map(|output| ToolOutput {
            trust: Trust::Untrusted,
            cost_usd_micros: output.cost_usd_micros.max(metered),
            ..output
        })
    }
}
