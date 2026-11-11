//! Requests and replies, in the brain's own types.

use serde_json::Value;

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The harness's instructions.
    System,
    /// The person, or the harness on their behalf.
    User,
    /// The model.
    Assistant,
    /// A tool's result.
    Tool,
}

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who wrote it.
    pub role: Role,
    /// Its text.
    pub content: String,
    /// The tools a model's message called.
    pub tool_calls: Vec<ToolCall>,
    /// The call a tool's message answers.
    pub tool_call_id: Option<String>,
}

impl Message {
    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Instructions from the harness.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    /// A message to the model.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    /// A message from the model.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    /// The result of a tool call.
    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            ..Self::text(Role::Tool, content)
        }
    }
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Its name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: Value,
}

/// A tool call the model asked for. Nothing about it has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The id its result must quote.
    pub id: String,
    /// The tool it named, which may not exist.
    pub name: String,
    /// Its arguments, as the raw text the model wrote.
    pub arguments: String,
}

/// A schema the reply must meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseSchema {
    /// A short name for it.
    pub name: String,
    /// The JSON Schema.
    pub schema: Value,
}

/// One call to a chat model.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// The conversation.
    pub messages: Vec<Message>,
    /// The tools offered.
    pub tools: Vec<ToolSpec>,
    /// The most the reply may be. Cut to the chosen model's own limit.
    pub max_output_tokens: u32,
    /// Sampling temperature, if the caller has a view.
    pub temperature: Option<f32>,
    /// A schema for the reply. A model with the `json_schema` capability has
    /// it enforced by the server; other models are told it in a message.
    pub response_schema: Option<ResponseSchema>,
}

/// Why a reply ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished.
    Stop,
    /// It hit the output limit, so the text is cut.
    Length,
    /// It wants tools run.
    ToolCalls,
    /// Anything else, or nothing stated.
    Other,
}

/// Tokens a call used, as the server reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Tokens read.
    pub input_tokens: u32,
    /// Tokens written.
    pub output_tokens: u32,
    /// What the server says the call cost, in millionths of a dollar, if it
    /// says. A router that sends calls to several upstreams at different
    /// prices reports this, and it can be more than the listed price.
    pub reported_cost_usd_micros: Option<u64>,
}

/// A model's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResponse {
    /// Its text, possibly empty.
    pub content: String,
    /// The tools it asked for.
    pub tool_calls: Vec<ToolCall>,
    /// Why it ended.
    pub finish: FinishReason,
    /// What it used. Absent when the server didn't say.
    pub usage: Option<Usage>,
}

/// Overhead counted for each message's role and framing.
const TOKENS_PER_MESSAGE: u32 = 8;
/// Bytes per token. Deliberately low, to overestimate.
const BYTES_PER_TOKEN: usize = 3;

/// A deliberately high guess at a request's input tokens, used to check that
/// it fits a model and to bound what it could cost.
#[must_use]
pub fn estimate_tokens(request: &ChatRequest) -> u32 {
    let mut bytes = 0usize;
    let mut messages = 0u32;
    for message in &request.messages {
        messages = messages.saturating_add(1);
        bytes = bytes.saturating_add(message.content.len());
        for call in &message.tool_calls {
            bytes = bytes
                .saturating_add(call.name.len())
                .saturating_add(call.arguments.len());
        }
    }
    for tool in &request.tools {
        bytes = bytes
            .saturating_add(tool.name.len())
            .saturating_add(tool.description.len())
            .saturating_add(tool.parameters.to_string().len());
    }
    if let Some(schema) = &request.response_schema {
        bytes = bytes.saturating_add(schema.schema.to_string().len());
    }
    u32::try_from(bytes.div_ceil(BYTES_PER_TOKEN))
        .unwrap_or(u32::MAX)
        .saturating_add(messages.saturating_mul(TOKENS_PER_MESSAGE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_estimate_counts_everything_sent() {
        let bare = ChatRequest {
            messages: vec![Message::user("abc")],
            tools: vec![],
            max_output_tokens: 1,
            temperature: None,
            response_schema: None,
        };
        assert_eq!(estimate_tokens(&bare), 1 + TOKENS_PER_MESSAGE);
        let with_schema = ChatRequest {
            response_schema: Some(ResponseSchema {
                name: "s".to_owned(),
                schema: serde_json::json!({ "type": "object" }),
            }),
            ..bare.clone()
        };
        assert!(estimate_tokens(&with_schema) > estimate_tokens(&bare));
    }
}
