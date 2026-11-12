//! The `http` and `model` capabilities, as the host hands them out. The
//! runtime makes them; the host makes sure what they let in is labelled and
//! paid for, whatever the plugin says.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use plugin_api::{
    CapabilityUse, HttpCapability, JobsCapability, MemoryCapability, ModelCapability, Tool,
    ToolContext, ToolDefinition, ToolFailure, ToolOutput,
};
use protocol::{RunId, Trust};
use serde_json::Value;

/// What one run's capability calls have done and cost, not yet charged.
#[derive(Debug, Default)]
struct Tally {
    spent: u64,
    uses: Vec<CapabilityUse>,
}

/// What a plugin's capability calls have done and cost, per run, until the
/// tool call they were made in reports them.
#[derive(Debug, Default)]
pub struct CapabilityMeter {
    tallies: Mutex<HashMap<RunId, Tally>>,
    turns: Mutex<HashMap<RunId, Arc<tokio::sync::Mutex<()>>>>,
}

impl CapabilityMeter {
    fn tallies(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, Tally>> {
        self.tallies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the run's turn to spend. Holding it while checking what's
    /// left and making the call means two calls at once can't both spend
    /// the same remainder.
    pub async fn turn(&self, run: RunId) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = Arc::clone(
            self.turns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(run)
                .or_default(),
        );
        turn.lock_owned().await
    }

    /// Records a capability call, and adds what it cost to its run.
    pub fn record(&self, run: RunId, used: CapabilityUse) {
        let mut tallies = self.tallies();
        let tally = tallies.entry(run).or_default();
        tally.spent = tally.spent.saturating_add(used.cost_usd_micros);
        tally.uses.push(used);
    }

    /// What the run has spent and not yet been charged.
    #[must_use]
    pub fn pending(&self, run: RunId) -> u64 {
        self.tallies().get(&run).map_or(0, |tally| tally.spent)
    }

    /// How many capability calls the run has made that aren't reported yet.
    #[must_use]
    pub fn calls(&self, run: RunId) -> u32 {
        self.tallies().get(&run).map_or(0, |tally| {
            u32::try_from(tally.uses.len()).unwrap_or(u32::MAX)
        })
    }

    /// Takes what the run has spent and done, to charge and report it on
    /// the call it was made for, such as a source's search.
    #[must_use]
    pub fn drain(&self, run: RunId) -> (u64, Vec<CapabilityUse>) {
        let tally = self.take(run);
        (tally.spent, tally.uses)
    }

    /// Takes what the run has done and spent, to report and charge it.
    fn take(&self, run: RunId) -> Tally {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        // Forget the run's turn unless a call is waiting on it or holds it.
        if turns
            .get(&run)
            .is_some_and(|turn| Arc::strong_count(turn) == 1)
        {
            turns.remove(&run);
        }
        drop(turns);
        self.tallies().remove(&run).unwrap_or_default()
    }
}

/// Where the `http` and `model` capabilities come from. The runtime gives
/// the host one; without it, a plugin asking for either is refused.
pub trait CapabilitySource: Send + Sync + std::fmt::Debug {
    /// HTTP for the plugin named. Each request is recorded in `meter`, and
    /// counts as one of the run's tool calls.
    fn http(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn HttpCapability>;

    /// Model calls for the plugin named. Each call is recorded in `meter`
    /// with its cost, and may spend only what the run has left after what's
    /// pending there.
    fn model(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn ModelCapability>;

    /// Long-term memory for the plugin named. Each note stored is recorded
    /// in `meter` with what its embedding cost, and counts as a tool call.
    fn memory(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn MemoryCapability>;

    /// Starting workflow runs for the plugin named. Each start is recorded
    /// in `meter`, and counts as a tool call.
    fn jobs(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn JobsCapability>;
}

/// A tool of a plugin that holds `http` or `model`. What it returns came
/// from outside, or from a model, so it's untrusted; and what its model calls
/// cost goes on the run, even if the plugin reports less.
#[derive(Debug)]
pub(crate) struct GuardedTool {
    pub(crate) inner: Arc<dyn Tool>,
    pub(crate) meter: Option<Arc<CapabilityMeter>>,
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
        let tally = self
            .meter
            .as_ref()
            .map(|meter| meter.take(context.run))
            .unwrap_or_default();
        // What its capability calls did and cost goes on the run whether the
        // tool succeeded or not, and the host's record replaces the plugin's.
        match outcome {
            Ok(mut output) => {
                output.trust = Trust::Untrusted;
                output.cost_usd_micros = output.cost_usd_micros.max(tally.spent);
                output.uses = tally.uses;
                Ok(output)
            }
            Err(failure) => {
                let cost = failure.cost_usd_micros().max(tally.spent);
                Err(failure.with_cost(cost).with_uses(tally.uses))
            }
        }
    }
}
