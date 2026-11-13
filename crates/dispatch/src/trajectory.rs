//! Trajectories: what a run saw and did, in order, as Harness² reads it.
//!
//! Each event's body is a [`Step`], written and read here and nowhere else, so
//! a trajectory has one schema from the sink to the API.

use chrono::{DateTime, Utc};
use events::{EventError, EventKind, EventSink};
use model::Role;
use protocol::RunId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Budget, BudgetKind, CallStatus, Effect, Outcome, RefusalKind};

/// How many events are read from the sink at a time.
const PAGE: usize = 256;
/// How many characters of a harness id are kept.
const HARNESS_ID_CHARS: usize = 16;
/// The longest calls line, in characters.
const MAX_CALLS_LINE: usize = 4096;

/// What a run is, for the record: what ran, and under which harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSubject {
    /// `chat`, `agent:<name>` or `tool:<name>`.
    pub agent: String,
    /// The harness's id, from [`harness_id`].
    pub harness: String,
}

/// An id for a harness: anything that decides how a run behaves, such as its
/// instructions, tools and limits. Two runs with the same id had the same
/// harness, which is what lets Harness² compare them.
#[must_use]
pub fn harness_id(harness: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(harness).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    let mut id = String::with_capacity(HARNESS_ID_CHARS);
    for byte in digest.iter().take(HARNESS_ID_CHARS / 2) {
        id.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        id.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
    }
    id
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The harness's instructions.
    System,
    /// The person, or the harness on their behalf.
    User,
    /// The model.
    Assistant,
    /// A tool's result.
    Tool,
}

impl From<Role> for Speaker {
    fn from(role: Role) -> Self {
        match role {
            Role::System => Self::System,
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
            Role::Tool => Self::Tool,
        }
    }
}

/// The longest description of a capability call kept.
const MAX_WHAT: usize = 512;

/// One thing in a trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// The run began.
    Started {
        /// What ran.
        #[schemars(length(max = 128))]
        agent: String,
        /// The harness it ran under.
        #[schemars(length(max = 64))]
        harness: String,
        /// What the work was for.
        #[schemars(length(max = 128))]
        purpose: String,
        /// The tools it was offered.
        #[schemars(length(max = 64), inner(length(max = 64)))]
        offered: Vec<String>,
        /// Its limits.
        budget: Budget,
        /// The skills its instructions include.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16), inner(length(max = 64)))]
        skills: Vec<String>,
    },
    /// A message the model was given: at the start, such as its instructions
    /// or the question, or later by the harness, such as a guardrail's
    /// reminder.
    Message {
        /// Who wrote it.
        role: Speaker,
        /// Its text, cut if long.
        #[schemars(length(max = 20_000))]
        content: String,
        /// The tools it called, for a model's message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 512), inner(length(max = 64)))]
        tool_calls: Vec<String>,
    },
    /// A call to a model.
    Model {
        /// The model that answered, if one did.
        #[schemars(length(max = 256))]
        model: Option<String>,
        /// What it said, cut if long.
        #[schemars(length(max = 20_000))]
        reply: Option<String>,
        /// How many tools it asked for.
        tool_calls: u32,
        /// What it cost, in millionths of a dollar.
        cost_usd_micros: u64,
        /// Why it failed, if it did.
        #[schemars(length(max = 2048))]
        error: Option<String>,
        /// The prompt's tokens, as the model counted them or as estimated.
        /// Zero on a failed call, and in events written before 0.10.0.
        #[serde(default)]
        input_tokens: u64,
        /// The reply's tokens, likewise.
        #[serde(default)]
        output_tokens: u64,
        /// How long the call took, retries and fallbacks included.
        #[serde(default)]
        duration_ms: u64,
    },
    /// A call to a tool, whether it ran or was refused.
    Tool {
        /// Its place among the run's tool calls, from 0.
        index: u32,
        /// The tool asked for.
        #[schemars(length(max = 64))]
        tool: String,
        /// What became of it.
        status: CallStatus,
        /// Why it was refused, if it was.
        refusal: Option<RefusalKind>,
        /// Whether it reads or writes, once known.
        effect: Option<Effect>,
        /// True when the harness made the call, not the model.
        by_harness: bool,
        /// The arguments as sent, cut if long.
        #[schemars(length(max = 20_000))]
        arguments: String,
        /// What went back to the model, cut if long.
        #[schemars(length(max = 20_000))]
        reply: Option<String>,
        /// How long it ran.
        duration_ms: u64,
    },
    /// A call a plugin's tool made through a capability, under that tool
    /// call.
    Capability {
        /// The tool call's place among the run's tool calls.
        tool_index: u32,
        /// `http` or `model`.
        #[schemars(length(max = 32))]
        capability: String,
        /// The method and address, or the model that answered.
        #[schemars(length(max = MAX_WHAT))]
        what: String,
        /// Whether it worked.
        ok: bool,
        /// What it cost, in millionths of a dollar.
        cost_usd_micros: u64,
        /// How long it took.
        duration_ms: u64,
    },
    /// The run ended.
    Ended {
        /// How.
        outcome: Outcome,
        /// The limit it hit, if any.
        budget_hit: Option<BudgetKind>,
        /// What the whole run cost.
        cost_usd_micros: u64,
    },
}

impl Step {
    /// The step for a capability call made in the tool call at `tool_index`.
    #[must_use]
    pub fn capability(tool_index: u32, used: &plugin_api::CapabilityUse) -> Self {
        let capability = serde_json::to_value(used.capability)
            .ok()
            .and_then(|name| name.as_str().map(str::to_owned))
            .unwrap_or_default();
        Self::Capability {
            tool_index,
            capability,
            what: used.what.chars().take(MAX_WHAT).collect(),
            ok: used.ok,
            cost_usd_micros: used.cost_usd_micros,
            duration_ms: used.duration_ms,
        }
    }

    /// The sink's kind for this step.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::Started { .. } => EventKind::RunStarted,
            Self::Message { .. } => EventKind::Message,
            Self::Model { .. } => EventKind::ModelCall,
            Self::Tool { .. } | Self::Capability { .. } => EventKind::ToolCall,
            Self::Ended { .. } => EventKind::RunEnded,
        }
    }

    /// The step as an event body.
    #[must_use]
    pub fn body(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A step and when it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TimedStep {
    /// When.
    pub at: DateTime<Utc>,
    /// What.
    pub step: Step,
}

/// A run's trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Trajectory {
    /// The run.
    #[schemars(length(max = 36))]
    pub run: String,
    /// What ran, if the run's start was recorded.
    #[schemars(length(max = 128))]
    pub agent: Option<String>,
    /// The harness it ran under.
    #[schemars(length(max = 64))]
    pub harness: Option<String>,
    /// The run in one line: each tool call and how it went, then how the run
    /// ended. Read this first; it keeps runs from being confused.
    #[schemars(length(max = 4200))]
    pub calls: String,
    /// What happened, in order.
    #[schemars(length(max = 2048))]
    pub steps: Vec<TimedStep>,
    /// Events that couldn't be read as steps, such as ones written before
    /// steps had a schema.
    pub unreadable: u32,
    /// False when the run had more steps than were read.
    pub complete: bool,
}

fn status_word(status: CallStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn calls_line(steps: &[TimedStep], complete: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for timed in steps {
        match &timed.step {
            Step::Tool {
                tool,
                status,
                by_harness,
                ..
            } => {
                let by = if *by_harness { " (harness)" } else { "" };
                parts.push(format!("{tool} {}{by}", status_word(*status)));
            }
            Step::Ended { outcome, .. } => {
                let word = serde_json::to_value(outcome)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                parts.push(word);
            }
            _ => {}
        }
    }
    if !complete {
        parts.push("(more not read)".to_owned());
    }
    if parts.is_empty() {
        return "no tool calls, not ended".to_owned();
    }
    let mut line = parts.join(" → ");
    if line.chars().count() > MAX_CALLS_LINE {
        line = line.chars().take(MAX_CALLS_LINE).collect();
        line.push_str("… (cut)");
    }
    line
}

/// Reads a run's trajectory, up to `max_steps` steps. `None` if the sink has
/// nothing for the run.
///
/// # Errors
///
/// [`EventError`] if the sink can't be read.
pub async fn read_trajectory(
    sink: &dyn EventSink,
    run: RunId,
    max_steps: usize,
) -> Result<Option<Trajectory>, EventError> {
    let mut steps = Vec::new();
    let mut unreadable = 0_u32;
    let mut seen = 0_usize;
    let mut complete = true;
    loop {
        let room = max_steps.saturating_sub(seen);
        if room == 0 {
            complete = sink.read_run(run, seen, 1).await?.events.is_empty();
            break;
        }
        let page = sink.read_run(run, seen, room.min(PAGE)).await?;
        seen += page.events.len();
        for event in page.events {
            match serde_json::from_value::<Step>(event.body) {
                Ok(step) if step.kind() == event.kind => {
                    steps.push(TimedStep { at: event.at, step });
                }
                _ => unreadable = unreadable.saturating_add(1),
            }
        }
        if !page.has_more {
            break;
        }
    }
    if seen == 0 {
        return Ok(None);
    }
    let (agent, harness) = steps
        .iter()
        .find_map(|timed| match &timed.step {
            Step::Started { agent, harness, .. } => Some((agent.clone(), harness.clone())),
            _ => None,
        })
        .unzip();
    Ok(Some(Trajectory {
        run: run.to_string(),
        agent,
        harness,
        calls: calls_line(&steps, complete),
        steps,
        unreadable,
        complete,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use events::{Event, MemorySink};
    use protocol::DataClass;
    use serde_json::json;

    fn budget() -> Budget {
        Budget {
            max_rounds: 1,
            max_tool_calls: 1,
            max_writes: 0,
            max_total_tokens: 100,
            max_usd_micros: 0,
            max_request_bytes: 1000,
            max_duration_ms: 1000,
        }
    }

    fn tool(index: u32, name: &str, status: CallStatus, by_harness: bool) -> Step {
        Step::Tool {
            index,
            tool: name.to_owned(),
            status,
            refusal: None,
            effect: None,
            by_harness,
            arguments: "{}".to_owned(),
            reply: None,
            duration_ms: 0,
        }
    }

    async fn sink_with(run: RunId, steps: &[Step]) -> MemorySink {
        let sink = MemorySink::new();
        let events = steps
            .iter()
            .map(|step| Event {
                run,
                at: DateTime::UNIX_EPOCH,
                kind: step.kind(),
                data_class: DataClass::Private,
                body: step.body(),
            })
            .collect();
        sink.append(events).await.unwrap();
        sink
    }

    #[test]
    fn a_harness_id_is_stable_and_tells_harnesses_apart() {
        let one = harness_id(&json!({ "prompt": "a" }));
        assert_eq!(one, harness_id(&json!({ "prompt": "a" })));
        assert_ne!(one, harness_id(&json!({ "prompt": "b" })));
        assert_eq!(one.len(), HARNESS_ID_CHARS);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn a_trajectory_reads_back_with_its_calls_line() {
        let run = RunId::generate();
        let steps = [
            Step::Started {
                agent: "chat".to_owned(),
                harness: "abc".to_owned(),
                purpose: "chat".to_owned(),
                offered: vec!["memory".to_owned()],
                budget: budget(),
                skills: Vec::new(),
            },
            tool(0, "memory", CallStatus::Ran, true),
            Step::Message {
                role: Speaker::User,
                content: "hi".to_owned(),
                tool_calls: Vec::new(),
            },
            tool(1, "http_request", CallStatus::Refused, false),
            Step::Ended {
                outcome: Outcome::Answered,
                budget_hit: None,
                cost_usd_micros: 0,
            },
        ];
        let sink = sink_with(run, &steps).await;
        let read = read_trajectory(&sink, run, 100).await.unwrap().unwrap();
        assert_eq!(read.agent.as_deref(), Some("chat"));
        assert_eq!(read.harness.as_deref(), Some("abc"));
        assert_eq!(
            read.calls,
            "memory ran (harness) → http_request refused → answered"
        );
        let read_back: Vec<Step> = read.steps.into_iter().map(|timed| timed.step).collect();
        assert_eq!(read_back, steps);
        assert!(read.complete);
        assert_eq!(read.unreadable, 0);
    }

    #[tokio::test]
    async fn a_long_run_says_it_was_not_read_whole() {
        let run = RunId::generate();
        let steps: Vec<Step> = (0..5)
            .map(|index| tool(index, "memory", CallStatus::Ran, false))
            .collect();
        let sink = sink_with(run, &steps).await;
        let read = read_trajectory(&sink, run, 3).await.unwrap().unwrap();
        assert_eq!(read.steps.len(), 3);
        assert!(!read.complete);
        assert!(read.calls.ends_with("(more not read)"));
    }

    #[tokio::test]
    async fn an_event_that_is_not_a_step_is_counted_not_hidden() {
        let run = RunId::generate();
        let sink = MemorySink::new();
        sink.append(vec![Event {
            run,
            at: DateTime::UNIX_EPOCH,
            kind: EventKind::ToolCall,
            data_class: DataClass::Private,
            body: json!({ "tool": "memory" }),
        }])
        .await
        .unwrap();
        let read = read_trajectory(&sink, run, 10).await.unwrap().unwrap();
        assert_eq!(read.unreadable, 1);
        assert_eq!(read.steps, vec![]);
        assert_eq!(read.agent, None);
    }

    #[tokio::test]
    async fn an_unknown_run_has_no_trajectory() {
        let sink = MemorySink::new();
        assert_eq!(
            read_trajectory(&sink, RunId::generate(), 10).await.unwrap(),
            None
        );
    }
}
