//! What a client reaches over MCP. Every call is its own run, with its own
//! budget and receipt, through the same dispatch as a model's calls.

use async_trait::async_trait;
use dispatch::{
    Budget, CallStatus, ChatResult, Outcome, Receipt, RunSubject, Session, SessionConfig,
    SessionError, harness_id,
};
use model::{Message, ToolCall};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::{RunId, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::Runtime;

/// What chats are for, which decides whether they may pay.
const CHAT_PURPOSE: &str = "chat";
/// What direct tool calls are for.
const TOOL_PURPOSE: &str = "tool";
/// A client isn't a small model, so it can take a whole result.
const DIRECT_RESULT_CHARS: usize = 500_000;
const MAX_MESSAGE: usize = 32_000;
const MAX_ANSWER: usize = 1_000_000;
const MAX_REASON: usize = 2048;

const CHAT_DESCRIPTION: &str = "Ask the assistant something, such as where a matter stands. It looks \
up what the question needs in what it's set up to read (memory, records, the web), answers from \
that, and returns the answer with a receipt of what actually ran.";

pub(crate) fn setup_failed(error: &SessionError) -> ToolFailure {
    ToolFailure::new(schema::bounded(&error.to_string()))
}

fn session_for(
    runtime: &Runtime,
    offered: Vec<String>,
    purpose: &str,
    max_result_chars: usize,
    subject: RunSubject,
) -> Result<Session, ToolFailure> {
    session_with(
        runtime,
        offered,
        runtime.config.budget,
        purpose,
        max_result_chars,
        subject,
        None,
    )
}

pub(crate) fn session_with(
    runtime: &Runtime,
    offered: Vec<String>,
    budget: Budget,
    purpose: &str,
    max_result_chars: usize,
    subject: RunSubject,
    probe: Option<dispatch::Probe>,
) -> Result<Session, ToolFailure> {
    Session::new(
        Arc::clone(&runtime.registry),
        Arc::clone(&runtime.sink),
        SessionConfig::new(RunId::generate(), subject, budget)
            .offering(offered)
            .with_purpose(purpose.to_owned())
            .with_max_result_chars(max_result_chars)
            .with_probe(probe),
    )
    .map(|session| session.with_stop(runtime.workflows.stop.clone()))
    .map_err(|error| setup_failed(&error))
}

/// What a chat starts from.
#[derive(Debug)]
pub(crate) struct ChatInput {
    /// Added after the runtime's own instructions, never in place of them.
    pub(crate) extra_system: Option<String>,
    /// Earlier turns, oldest first.
    pub(crate) history: Vec<Message>,
    /// The latest thing asked.
    pub(crate) question: String,
    /// Sampling temperature, if the client has a view.
    pub(crate) temperature: Option<f32>,
    /// The longest each reply may be.
    pub(crate) max_output_tokens: u32,
    /// The most time the run may take, when something outside it, such as
    /// a job's limit, would stop it sooner than its own budget.
    pub(crate) time_cap_ms: Option<u64>,
    /// Who asked, if it's known: the actor recorded on the run.
    pub(crate) requested_by: Option<String>,
    /// True if the run is private to whoever asked.
    pub(crate) private: bool,
}

/// Who's calling a core tool over HTTP: their actor, and whether what they
/// start is private by default.
#[derive(Debug, Clone)]
pub(crate) struct Asking {
    pub(crate) actor: String,
    pub(crate) private_by_default: bool,
}

tokio::task_local! {
    /// Who's calling a core tool over HTTP. A tool's signature can't carry
    /// it (a plugin's `ToolContext` is `Copy`), so the HTTP layer sets it
    /// around one call, and the core's own tools read it as they start and
    /// put it on the run. Nothing deeper reads it.
    pub(crate) static CALLER: Asking;
}

/// Who's calling, if the HTTP layer said.
fn caller() -> Option<Asking> {
    CALLER.try_with(Clone::clone).ok()
}

/// What a client asks.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatArgs {
    /// The question or request.
    #[schemars(length(min = 1, max = MAX_MESSAGE))]
    pub message: String,
    /// True to keep the run to whoever asked, and the owner: its words never
    /// reach Harness². Left out, the key's default decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

/// How a chat ended.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ChatReply {
    /// The answer. Only a run that answered has one.
    #[schemars(length(max = MAX_ANSWER))]
    pub answer: Option<String>,
    /// Why there's no answer.
    #[schemars(length(max = MAX_REASON))]
    pub stopped_because: Option<String>,
    /// What actually happened.
    pub receipt: Receipt,
}

impl From<ChatResult> for ChatReply {
    fn from(result: ChatResult) -> Self {
        Self {
            answer: result.answer,
            stopped_because: result.stopped_because,
            receipt: result.receipt,
        }
    }
}

/// One conversation with the runtime.
#[derive(Debug)]
pub(crate) struct ChatTool(Runtime);

impl ChatTool {
    pub(crate) const fn new(runtime: Runtime) -> Self {
        Self(runtime)
    }
}

#[async_trait]
impl Tool for ChatTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<ChatArgs, ChatReply>("chat", CHAT_DESCRIPTION, Effect::Write)
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: ChatArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let asking = caller();
        let private = args.private.unwrap_or_else(|| {
            asking
                .as_ref()
                .is_some_and(|asking| asking.private_by_default)
        });
        let result = self
            .0
            .chat(
                args.message,
                CHAT_PURPOSE,
                None,
                asking.map(|asking| asking.actor),
                private,
            )
            .await?;
        let cost_usd_micros = result.receipt.totals.cost_usd_micros;
        let reply = ChatReply::from(result);
        let value = serde_json::to_value(reply)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        // A model wrote the answer, after reading whatever it fetched.
        Ok(ToolOutput::new(value, Trust::Untrusted).with_cost(cost_usd_micros))
    }
}

impl Runtime {
    /// Runs one chat, as its own run, on behalf of `purpose`, in at most
    /// `time_cap_ms` if that's given, for `requested_by` if it's known.
    pub(crate) async fn chat(
        &self,
        question: String,
        purpose: &str,
        time_cap_ms: Option<u64>,
        requested_by: Option<String>,
        private: bool,
    ) -> Result<ChatResult, ToolFailure> {
        let input = ChatInput {
            extra_system: None,
            history: Vec::new(),
            question,
            temperature: None,
            max_output_tokens: self.config.max_output_tokens,
            time_cap_ms,
            requested_by,
            private,
        };
        self.chat_with(input, purpose).await
    }

    /// A chat that continues a conversation the client kept. It's always
    /// a routed turn: by `front_door`, or without it by the route of every
    /// profile defined.
    pub(crate) async fn chat_with(
        &self,
        input: ChatInput,
        purpose: &str,
    ) -> Result<ChatResult, ToolFailure> {
        let line = match self.config.route() {
            Some(route) => self.line_of(route).await?,
            None => self.default_line().await?,
        };
        self.routed(&line, input, purpose).await
    }
}

/// What a tool's result looks like to a client. It's dispatch's reply.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolEnvelope {
    /// `ok`.
    #[schemars(length(max = 16))]
    pub status: String,
    /// True if the result came from outside, and mustn't be taken as
    /// instructions.
    pub untrusted: bool,
    /// The tool's result.
    #[schemars(extend("x-extension" = "the tool's own result, checked against its schema"))]
    pub result: Option<Value>,
    /// True if only part of the result is here.
    pub truncated: Option<bool>,
    /// How much of it is here, in characters.
    pub shown_chars: Option<u64>,
    /// How long it is, in characters.
    pub total_chars: Option<u64>,
    /// The part of it that's here.
    #[schemars(length(max = DIRECT_RESULT_CHARS))]
    pub partial_result: Option<String>,
}

/// One bundled tool, called directly by a client.
#[derive(Debug)]
pub(crate) struct DispatchedTool {
    runtime: Runtime,
    definition: ToolDefinition,
}

impl DispatchedTool {
    pub(crate) const fn new(runtime: Runtime, definition: ToolDefinition) -> Self {
        Self {
            runtime,
            definition,
        }
    }
}

#[async_trait]
impl Tool for DispatchedTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.definition.clone();
        definition.input_schema = schema::for_model(&self.definition.input_schema);
        definition.output_schema = schema::schema_of::<ToolEnvelope>();
        definition
    }

    fn effect_of(&self, arguments: &Value) -> Effect {
        self.runtime
            .registry
            .effect_of(&self.definition.name, arguments)
            .unwrap_or(self.definition.effect)
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let name = self.definition.name.clone();
        let subject = RunSubject::new(
            format!("tool:{name}"),
            harness_id(&json!({ "tool": name, "budget": self.runtime.config.budget })),
        )
        .requested_by(caller().map(|asking| asking.actor))
        .private(caller().is_some_and(|asking| asking.private_by_default))
        .paid_by(
            self.runtime
                .payer_for(caller().map(|asking| asking.actor).as_deref())
                .await,
        );
        let mut session = session_for(
            &self.runtime,
            vec![name.clone()],
            TOOL_PURPOSE,
            DIRECT_RESULT_CHARS,
            subject,
        )?;
        let call = ToolCall {
            id: "direct".to_owned(),
            name,
            arguments: arguments.to_string(),
        };
        let reply = session.dispatch(&call).await;
        let claimed = match reply.status {
            CallStatus::Ran => Outcome::Direct,
            // A client's direct call is never a probe.
            CallStatus::Refused | CallStatus::Stubbed => Outcome::Refused,
            CallStatus::Failed | CallStatus::TimedOut => Outcome::ToolFailed,
        };
        let receipt = session.finish(claimed).await;
        if reply.status != CallStatus::Ran {
            return Err(ToolFailure::new(reply.content));
        }
        let value: Value = serde_json::from_str(&reply.content)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let untrusted = value.get("untrusted").and_then(Value::as_bool);
        Ok(ToolOutput::new(
            value,
            if untrusted == Some(false) {
                Trust::Trusted
            } else {
                Trust::Untrusted
            },
        )
        .with_cost(receipt.totals.cost_usd_micros))
    }
}
