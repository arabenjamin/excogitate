//! What a client reaches over MCP. Every call is its own run, with its own
//! budget and receipt, through the same dispatch as a model's calls.

use async_trait::async_trait;
use dispatch::{
    Budget, CallStatus, ChatResult, Conversation, Outcome, Receipt, RunSubject, Session,
    SessionConfig, SessionError, harness_id,
};
use memory::ChoiceHeld;
use model::{Message, ToolCall};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::{DataClass, RunId, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use workflow::DELEGATE_TOOL;

use crate::{Config, Runtime};

/// What chats are for, which decides whether they may pay.
const CHAT_PURPOSE: &str = "chat";
/// What direct tool calls are for.
const TOOL_PURPOSE: &str = "tool";
/// A client isn't a small model, so it can take a whole result.
const DIRECT_RESULT_CHARS: usize = 500_000;
const MAX_MESSAGE: usize = 32_000;
/// The words of the turn that holds the harness's search. A small model
/// often returned nothing at all when this turn was blank.
const RECALL_TURN: &str = "Checking my memory first.";
/// The longest query `memory` takes.
const MAX_RECALL_QUERY: usize = 1024;
const MAX_ANSWER: usize = 1_000_000;
const MAX_REASON: usize = 2048;

const CHAT_DESCRIPTION: &str = "Ask the assistant something. It can read URLs and use its long-term \
memory, and it returns its answer with a receipt of what actually ran.";

fn setup_failed(error: &SessionError) -> ToolFailure {
    ToolFailure::new(schema::bounded(&error.to_string()))
}

/// What decides how a chat behaves, for its harness id. A client's own
/// instructions are part of its input, not of the harness.
fn chat_subject(config: &Config, offered: &[String]) -> RunSubject {
    RunSubject {
        agent: "chat".to_owned(),
        harness: harness_id(&json!({
            "system_prompt": config.system_prompt,
            "offered": offered,
            "budget": config.budget,
            "max_result_chars": config.max_result_chars,
            "recall_limit": config.recall_limit,
            "recall_min_similarity": config.recall_min_similarity,
        })),
    }
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
    )
}

fn session_with(
    runtime: &Runtime,
    offered: Vec<String>,
    budget: Budget,
    purpose: &str,
    max_result_chars: usize,
    subject: RunSubject,
) -> Result<Session, ToolFailure> {
    Session::new(
        Arc::clone(&runtime.registry),
        Arc::clone(&runtime.sink),
        SessionConfig {
            run: RunId::generate(),
            offered,
            budget,
            data_class: DataClass::Private,
            purpose: purpose.to_owned(),
            max_result_chars,
            subject,
            probe: None,
        },
    )
    .map(|session| session.with_stop(runtime.workflows.stop.clone()))
    .map_err(|error| setup_failed(&error))
}

/// The harness's own memory search, before a chat's first round. A small
/// model often doesn't think to look. What it finds joins the transcript as a
/// call already made, and if nothing is close enough the model sees nothing,
/// because irrelevant notes make it say it doesn't know things it does.
async fn recall(
    config: &Config,
    session: &mut Session,
    question: &str,
    messages: &mut Vec<Message>,
) {
    // A run that isn't offered memory, like a front door that delegates
    // retrieval, gets no recall: the call would only be refused.
    if config.recall_limit == 0 || !session.offers("memory") {
        return;
    }
    let query: String = question.chars().take(MAX_RECALL_QUERY).collect();
    let mut arguments = json!({
        "operation": "search",
        "query": query,
        "limit": config.recall_limit,
    });
    if config.recall_min_similarity > 0.0
        && let Some(fields) = arguments.as_object_mut()
    {
        fields.insert(
            "min_similarity".to_owned(),
            json!(config.recall_min_similarity),
        );
    }
    let call = ToolCall {
        id: "recall".to_owned(),
        name: "memory".to_owned(),
        arguments: arguments.to_string(),
    };
    let Some(reply) = session.dispatch_for_harness(&call).await else {
        return;
    };
    if reply.status == CallStatus::Ran && found_something(&reply.content) {
        let mut assistant = Message::assistant(RECALL_TURN);
        assistant.tool_calls.push(call);
        messages.push(assistant);
        messages.push(Message::tool(reply.call_id, reply.content));
    }
}

/// True unless the reply is a search that found nothing.
fn found_something(reply: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(reply).ok();
    let hits = parsed
        .as_ref()
        .and_then(|value| value.pointer("/result/hits"));
    !hits.and_then(Value::as_array).is_some_and(Vec::is_empty)
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
}

/// What a client asks.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatArgs {
    /// The question or request.
    #[schemars(length(min = 1, max = MAX_MESSAGE))]
    pub message: String,
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
        let result = self.0.chat(args.message, CHAT_PURPOSE, None).await?;
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
    /// `time_cap_ms` if that's given.
    pub(crate) async fn chat(
        &self,
        question: String,
        purpose: &str,
        time_cap_ms: Option<u64>,
    ) -> Result<ChatResult, ToolFailure> {
        let input = ChatInput {
            extra_system: None,
            history: Vec::new(),
            question,
            temperature: None,
            max_output_tokens: self.config.max_output_tokens,
            time_cap_ms,
        };
        self.chat_with(input, purpose).await
    }

    /// A chat that continues a conversation the client kept.
    pub(crate) async fn chat_with(
        &self,
        input: ChatInput,
        purpose: &str,
    ) -> Result<ChatResult, ToolFailure> {
        let ChatInput {
            extra_system,
            history,
            question,
            temperature,
            max_output_tokens,
            time_cap_ms,
        } = input;
        // `_chosen` holds the front door's choice of sources for the chat.
        let (mut session, instructions, _chosen) = if let Some(name) = &self.config.front_door {
            self.front_door(name, purpose, time_cap_ms).await?
        } else {
            // Without a front door there's no roster, so nothing to delegate to.
            let offered: Vec<String> = self
                .registry
                .definitions()
                .into_iter()
                .map(|definition| definition.name)
                .filter(|name| name != DELEGATE_TOOL)
                .collect();
            let subject = chat_subject(&self.config, &offered);
            let session = session_for(
                self,
                offered,
                purpose,
                self.config.max_result_chars,
                subject,
            )?;
            (session, self.config.system_prompt.clone(), None)
        };
        let system = match extra_system {
            Some(extra) => format!("{instructions}\n\n{extra}"),
            None => instructions,
        };
        let mut messages = vec![Message::system(system)];
        messages.extend(history);
        messages.push(Message::user(question.clone()));
        // The task goes on the trajectory before the search made for it.
        session.begin(&messages).await;
        recall(&self.config, &mut session, &question, &mut messages).await;
        let conversation = Conversation {
            messages,
            max_output_tokens,
            temperature,
        };
        Ok(session.converse(&self.gateway, conversation).await)
    }
}

impl Runtime {
    /// A chat's session and instructions when an agent is the front door:
    /// its tools, its budget, and its instructions with the roster added.
    /// Its runs are that agent's, so Harness² can improve it like any other.
    async fn front_door(
        &self,
        name: &str,
        purpose: &str,
        time_cap_ms: Option<u64>,
    ) -> Result<(Session, String, Option<ChoiceHeld>), ToolFailure> {
        let agent = self
            .workflows
            .agent(name)
            .await
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?
            .ok_or_else(|| {
                ToolFailure::new(format!(
                    "front_door names the agent {name}, and there's no such agent"
                ))
            })?;
        let subject = RunSubject {
            agent: format!("agent:{name}"),
            harness: self.workflows.agent_harness(&agent),
        };
        let mut budget = agent.budget.unwrap_or(self.config.budget);
        // Held to what's outside the run, so it ends itself, with a receipt,
        // before something else stops it.
        if let Some(cap) = time_cap_ms {
            budget.max_duration_ms = budget.max_duration_ms.min(cap);
        }
        let instructions = self
            .workflows
            .instructions(name, &agent)
            .await
            .map_err(ToolFailure::new)?;
        let session = session_with(
            self,
            agent.tools.clone(),
            budget,
            purpose,
            self.config.max_result_chars,
            subject,
        )?;
        let chosen = self.workflows.choose_sources(session.run(), &agent);
        Ok((session, instructions, chosen))
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

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let name = self.definition.name.clone();
        let subject = RunSubject {
            agent: format!("tool:{name}"),
            harness: harness_id(&json!({ "tool": name, "budget": self.runtime.config.budget })),
        };
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
