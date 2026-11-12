//! `delegate`: hands a task to another agent, which runs as its own run with
//! its own tools and budget, and returns its answer. It's how a front door
//! with a few tools of its own reaches everything else.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use dispatch::{Outcome, Probe, StopSignal, ToolRegistry};
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
pub(crate) struct DelegateArgs {
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

/// The longest answer passed back; a longer one is cut to this and marked.
const MAX_ANSWER_CHARS: usize = 200_000;
/// The longest reason for stopping, as the result's schema allows.
const MAX_STOPPED_CHARS: usize = 2048;

/// An answer cut to fit the result's schema, and whether it was: a long
/// answer arrives marked instead of failing the call and losing its cost.
fn fitted(answer: Option<String>) -> (Option<String>, bool) {
    match answer {
        Some(answer) if answer.chars().count() > MAX_ANSWER_CHARS => {
            (Some(answer.chars().take(MAX_ANSWER_CHARS).collect()), true)
        }
        answer => (answer, false),
    }
}

/// What comes back.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct DelegateResult {
    /// The agent that ran.
    #[schemars(length(max = 64))]
    agent: String,
    /// Its answer, if it gave one.
    #[schemars(length(max = 200_000))]
    answer: Option<String>,
    /// True when the answer was longer than this tool passes back, and was
    /// cut to fit. The whole answer is in its run.
    answer_cut_off: bool,
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
    pub(crate) stop: StopSignal,
    /// The reads a probed hand-off may really make, as Harness² allows.
    pub(crate) probe_reads: BTreeSet<String>,
    /// The sources each agent's run may read.
    pub(crate) sources: workflow::SourceChoices,
}

impl DelegateTool {
    fn engine(&self) -> Option<Engine> {
        Some(Engine {
            registry: self.registry.upgrade()?,
            gateway: Arc::clone(&self.gateway),
            sink: Arc::clone(&self.sink),
            store: Arc::clone(&self.store),
            limits: self.limits,
            stop: self.stop.clone(),
            sources: self.sources.clone(),
        })
    }
}

/// How long a hand-off may take: what the caller has left, less enough for
/// the caller to read the answer and finish before its own limit.
fn within(time_left_ms: u64) -> Duration {
    let left = Duration::from_millis(time_left_ms);
    let margin = (left / 10).clamp(Duration::from_millis(200), Duration::from_secs(5));
    left.saturating_sub(margin)
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
        // A hand-off can lead to writes, so it counts as one. In a probe it
        // still runs, as a probe of the agent it goes to.
        ToolDefinition::for_types::<DelegateArgs, DelegateResult>(
            DELEGATE_TOOL,
            DESCRIPTION,
            Effect::Write,
        )
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: DelegateArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let engine = self
            .engine()
            .ok_or_else(|| ToolFailure::new("the runtime is shutting down".to_owned()))?;
        let result = engine
            .delegate(
                &args.agent,
                &args.task,
                args.context.as_deref(),
                context.budget_usd_micros,
                within(context.time_left_ms),
                context.probing.then(|| Probe {
                    reads: self.probe_reads.clone(),
                    nested: BTreeSet::new(),
                }),
            )
            .await
            .map_err(ToolFailure::new)?;
        let cost_usd_micros = result.receipt.totals.cost_usd_micros;
        let (answer, answer_cut_off) = fitted(result.answer);
        let reply = DelegateResult {
            agent: args.agent,
            outcome: outcome_name(result.receipt.outcome),
            run: result.receipt.run.clone(),
            answer,
            answer_cut_off,
            stopped_because: result
                .stopped_because
                .map(|why| why.chars().take(MAX_STOPPED_CHARS).collect()),
        };
        let value = serde_json::to_value(reply)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        // A model wrote it, after reading whatever its tools brought in.
        Ok(ToolOutput::new(value, Trust::Untrusted).with_cost(cost_usd_micros))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use axum::extract::State;
    use serde_json::json;

    use super::*;

    type Replies = Arc<Mutex<(VecDeque<Value>, Vec<Value>)>>;

    fn text(content: &str) -> Value {
        json!({ "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }] })
    }

    fn calls(name: &str, arguments: &Value) -> Value {
        json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [{
            "id": "c1", "type": "function",
            "function": { "name": name, "arguments": arguments.to_string() }
        }] }, "finish_reason": "tool_calls" }] })
    }

    /// A model that answers from a queue and keeps what it was sent.
    async fn model(replies: Vec<Value>) -> (String, Replies) {
        let state: Replies = Arc::new(Mutex::new((replies.into(), Vec::new())));
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(
                    |State(state): State<Replies>, body: axum::Json<Value>| async move {
                        let mut state = state.lock().unwrap();
                        state.1.push(body.0);
                        axum::Json(state.0.pop_front().unwrap_or_else(|| text("done")))
                    },
                ),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}/v1"), state)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_of_a_front_door_sees_its_roster_and_probes_the_hand_off() {
        let (url, state) = model(vec![
            calls(
                "delegate",
                &json!({ "agent": "helper", "task": "Save a note." }),
            ),
            calls("memory", &json!({ "operation": "store", "text": "otters" })),
            text("Saved."),
            text("The helper saved it."),
        ])
        .await;
        let yaml = format!(
            "version: 1\nproviders:\n  - {{name: c, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
             models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n"
        );
        let store = Arc::new(store_memory::MemoryStore::new(3));
        let runtime = crate::Runtime::assemble(
            Arc::clone(&store) as Arc<dyn Store>,
            Arc::new(events::MemorySink::new()),
            model::Catalog::from_yaml(&yaml, |_| None).unwrap(),
            crate::Config {
                front_door: Some("door".to_owned()),
                ..crate::Config::default()
            },
            &|_| None,
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!("probe-door-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in ["agents", "workflows", "schedules"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let door = "description: Talks.\nsystem_prompt: You are the door.\ntools: [delegate]\n";
        let helper = "description: Keeps notes.\nsystem_prompt: You keep notes.\ntools: [memory]\n";
        std::fs::write(root.join("agents/door.yaml"), door).unwrap();
        std::fs::write(root.join("agents/helper.yaml"), helper).unwrap();
        runtime
            .load_definitions(&root, chrono::Utc::now())
            .await
            .unwrap();
        let agent = runtime.workflows.agent("door").await.unwrap().unwrap();

        let probed = runtime
            .workflows
            .probe_agent("door", &agent, "Save otters.", BTreeSet::new())
            .await
            .unwrap();

        let seen = state.lock().unwrap().1.clone();
        // The door is probed with its roster, as it runs for real.
        let instructions = seen[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            instructions.contains("- helper: Keeps notes."),
            "{instructions}"
        );
        // The hand-off ran, as a probe of the helper.
        assert_eq!(probed.receipt.tool_calls[0].tool, "delegate");
        assert_eq!(
            probed.receipt.tool_calls[0].status,
            dispatch::CallStatus::Ran,
            "{:?}",
            probed.receipt
        );
        // The helper's write was stubbed, and nothing was stored.
        let helper_saw = seen[2]["messages"].to_string();
        assert!(helper_saw.contains("This is a test run"), "{helper_saw}");
        let notes = store::NoteReader::search_text(
            store.as_ref(),
            "otters",
            store::ClassFilter::Any,
            store::PageRequest::new(5, 0).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(notes.items, []);
    }

    #[test]
    fn a_long_answer_is_cut_to_fit_and_marked() {
        let long = "a".repeat(MAX_ANSWER_CHARS + 50_000);
        let (answer, cut) = fitted(Some(long));
        assert!(cut);
        assert_eq!(answer.as_ref().unwrap().chars().count(), MAX_ANSWER_CHARS);
        assert_eq!(
            fitted(Some("short".to_owned())),
            (Some("short".to_owned()), false)
        );
        assert_eq!(fitted(None), (None, false));
        // What it builds passes the result's published schema.
        let reply = DelegateResult {
            agent: "researcher".to_owned(),
            answer,
            answer_cut_off: cut,
            outcome: "answered".to_owned(),
            stopped_because: None,
            run: "r".to_owned(),
        };
        let output =
            schema::Validator::from_schema(&schema::schema_of::<DelegateResult>()).unwrap();
        output.check(&serde_json::to_value(reply).unwrap()).unwrap();
    }

    #[test]
    fn a_hand_off_ends_before_its_caller_does() {
        assert_eq!(within(10_000), Duration::from_secs(9));
        assert_eq!(within(1_000), Duration::from_millis(800));
        assert_eq!(within(600_000), Duration::from_secs(595));
        assert_eq!(within(100), Duration::ZERO);
    }
}
