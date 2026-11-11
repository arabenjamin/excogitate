//! The `OpenAI`-compatible front door. A client sees one model, and behind it
//! the whole brain: memory, tools, budgets and routing.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use dispatch::{ChatResult, Outcome, Receipt};
use futures_util::{StreamExt, stream};
use model::Message;
use schema::bounded;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, App, json_body};
use crate::mcp::ChatInput;

/// The one model a client sees.
pub(super) const MODEL_ID: &str = "brain";
const PURPOSE: &str = "chat";
const MAX_CONTENT: usize = 1_000_000;

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WireRole {
    /// Instructions.
    System,
    /// Instructions, under the newer name.
    Developer,
    /// The person.
    User,
    /// The model.
    Assistant,
    /// A tool's result, which this server refuses: its tools are its own.
    Tool,
}

/// One part of a message's content.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WirePart {
    /// Only text is accepted.
    #[schemars(regex(pattern = r"^text$"), length(max = 8))]
    pub r#type: String,
    /// The text.
    #[schemars(length(max = MAX_CONTENT))]
    pub text: String,
}

/// A message's content.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum WireContent {
    /// Plain text.
    Text(#[schemars(length(max = MAX_CONTENT))] String),
    /// A list of parts.
    Parts(#[schemars(length(max = 64))] Vec<WirePart>),
}

impl WireContent {
    fn text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Parts(parts) => parts
                .into_iter()
                .map(|part| part.text)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// One message from a client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WireMessage {
    /// Who wrote it.
    pub role: WireRole,
    /// What it says.
    pub content: Option<WireContent>,
    /// Refused if it holds anything: this server runs its own tools.
    #[schemars(extend("x-extension" = "refused unless empty"))]
    pub tool_calls: Option<Value>,
}

/// What an `OpenAI` client sends. Being someone else's format, fields it
/// doesn't use are ignored. Any that would change the meaning of the reply are
/// read, so they can be refused rather than quietly ignored.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[schemars(extend("x-extension" = "a client's other fields are ignored"))]
pub struct OpenAiRequest {
    /// Must be the model `/v1/models` lists.
    #[schemars(length(min = 1, max = 256))]
    pub model: String,
    /// The conversation so far, ending with the user.
    #[schemars(length(min = 1, max = 512))]
    pub messages: Vec<WireMessage>,
    /// Send the reply as server-sent events.
    pub stream: Option<bool>,
    /// The longest each reply may be. The server's own limit still applies.
    #[schemars(range(min = 1, max = 1_000_000))]
    pub max_tokens: Option<u32>,
    /// The same, under the newer name.
    #[schemars(range(min = 1, max = 1_000_000))]
    pub max_completion_tokens: Option<u32>,
    /// Sampling temperature.
    #[schemars(range(min = 0.0, max = 2.0))]
    pub temperature: Option<f32>,
    /// How many replies. Only 1.
    #[schemars(range(min = 1, max = 1))]
    pub n: Option<u32>,
    /// Refused if it holds anything: this server runs its own tools.
    #[schemars(extend("x-extension" = "refused unless empty"))]
    pub tools: Option<Value>,
    /// Refused unless it asks for plain text.
    #[schemars(extend("x-extension" = "only {\"type\": \"text\"} is accepted"))]
    pub response_format: Option<Value>,
}

fn invalid(message: &str) -> Response {
    wire_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        None,
    )
}

fn wire_error(
    status: StatusCode,
    kind: &str,
    message: &str,
    receipt: Option<&Receipt>,
) -> Response {
    (status, axum::Json(error_body(kind, message, receipt))).into_response()
}

fn error_body(kind: &str, message: &str, receipt: Option<&Receipt>) -> Value {
    json!({
        "error": { "message": bounded(message), "type": kind, "param": null, "code": null },
        "receipt": receipt,
    })
}

fn from_api(error: &ApiError) -> Response {
    wire_error(error.status(), "server_error", error.message(), None)
}

fn holds_something(field: Option<&Value>) -> bool {
    field.is_some_and(|value| value.as_array().is_none_or(|items| !items.is_empty()))
}

/// Turns a client's request into a chat, or says why it can't be one.
fn chat_input(request: OpenAiRequest, server_limit: u32) -> Result<ChatInput, String> {
    if request.model != MODEL_ID {
        return Err(format!("the only model here is {MODEL_ID}"));
    }
    if holds_something(request.tools.as_ref()) {
        return Err("this server runs its own tools, so `tools` must be empty".to_owned());
    }
    let plain = |format: &Value| format.get("type").and_then(Value::as_str) == Some("text");
    if request
        .response_format
        .as_ref()
        .is_some_and(|format| !plain(format))
    {
        return Err("only plain text replies are supported".to_owned());
    }
    let max_output_tokens = request
        .max_completion_tokens
        .or(request.max_tokens)
        .map_or(server_limit, |asked| asked.min(server_limit));
    let mut extra = Vec::new();
    let mut history = Vec::new();
    let mut turns = request.messages.into_iter().peekable();
    let mut question = None;
    while let Some(turn) = turns.next() {
        if holds_something(turn.tool_calls.as_ref()) {
            return Err(
                "this server runs its own tools, so a message can't hold tool calls".to_owned(),
            );
        }
        let text = turn.content.map(WireContent::text).unwrap_or_default();
        match turn.role {
            WireRole::System | WireRole::Developer => extra.push(text),
            WireRole::Tool => {
                return Err(
                    "this server runs its own tools, so a message can't be a tool's".to_owned(),
                );
            }
            WireRole::Assistant => history.push(Message::assistant(text)),
            WireRole::User if turns.peek().is_none() => question = Some(text),
            WireRole::User => history.push(Message::user(text)),
        }
    }
    let question = question
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "the last message must be from the user, and say something".to_owned())?;
    Ok(ChatInput {
        extra_system: (!extra.is_empty()).then(|| extra.join("\n\n")),
        history,
        question,
        temperature: request.temperature,
        max_output_tokens,
    })
}

/// How many tokens a run read and wrote, in the client's format.
fn usage(receipt: &Receipt) -> Value {
    let read: u64 = receipt
        .model_calls
        .iter()
        .map(|call| call.input_tokens)
        .sum();
    let wrote: u64 = receipt
        .model_calls
        .iter()
        .map(|call| call.output_tokens)
        .sum();
    json!({
        "prompt_tokens": read,
        "completion_tokens": wrote,
        "total_tokens": read.saturating_add(wrote),
    })
}

const fn finish_reason(receipt: &Receipt) -> &'static str {
    if receipt.answer_cut_off {
        "length"
    } else {
        "stop"
    }
}

fn identity(receipt: &Receipt) -> (String, i64) {
    (format!("chatcmpl-{}", receipt.run), Utc::now().timestamp())
}

/// The answer, or why there isn't one.
fn answered(result: ChatResult) -> Result<(String, Receipt), (String, Box<Receipt>)> {
    match (result.receipt.outcome, result.answer) {
        (Outcome::Answered, Some(answer)) => Ok((answer, result.receipt)),
        (_, _) => Err((
            result
                .stopped_because
                .unwrap_or_else(|| "the run ended without an answer".to_owned()),
            Box::new(result.receipt),
        )),
    }
}

fn whole_reply(result: ChatResult) -> Response {
    match answered(result) {
        Ok((answer, receipt)) => {
            let (id, created) = identity(&receipt);
            axum::Json(json!({
                "id": id,
                "object": "chat.completion",
                "created": created,
                "model": MODEL_ID,
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": answer },
                    "finish_reason": finish_reason(&receipt),
                }],
                "usage": usage(&receipt),
                "receipt": receipt,
            }))
            .into_response()
        }
        Err((why, receipt)) => {
            wire_error(StatusCode::BAD_GATEWAY, "run_stopped", &why, Some(&receipt))
        }
    }
}

fn event(data: &Value) -> Event {
    Event::default().data(data.to_string())
}

/// The events of a finished run. The brain doesn't stream tokens: the answer
/// is a single chunk, sent when the run is over.
fn stream_events(result: ChatResult) -> Vec<Event> {
    let mut events = match answered(result) {
        Ok((answer, receipt)) => {
            let (id, created) = identity(&receipt);
            let chunk = |delta: Value, finish: Value, extra: Value| {
                let mut body = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
                });
                if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
                    body.extend(extra.clone());
                }
                body
            };
            vec![
                event(&chunk(
                    json!({ "role": "assistant", "content": answer }),
                    Value::Null,
                    json!({}),
                )),
                event(&chunk(
                    json!({}),
                    json!(finish_reason(&receipt)),
                    json!({ "usage": usage(&receipt), "receipt": receipt }),
                )),
            ]
        }
        Err((why, receipt)) => vec![event(&error_body("run_stopped", &why, Some(&receipt)))],
    };
    events.push(Event::default().data("[DONE]"));
    events
}

async fn list_models() -> axum::Json<Value> {
    axum::Json(json!({
        "object": "list",
        "data": [{ "id": MODEL_ID, "object": "model", "created": 0, "owned_by": "local" }],
    }))
}

async fn chat_completions(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let raw = match json_body(&headers, &body) {
        Ok(raw) => raw,
        Err(error) => return from_api(&error),
    };
    let request: OpenAiRequest = match app.openai_request.parse(raw) {
        Ok(request) => request,
        Err(refusal) => return invalid(&refusal.to_string()),
    };
    let streaming = request.stream.unwrap_or(false);
    let server_limit = app.brain.config.max_output_tokens;
    let input = match chat_input(request, server_limit) {
        Ok(input) => input,
        Err(why) => return invalid(&why),
    };
    let permit = match app.permit() {
        Ok(permit) => permit,
        Err(error) => return from_api(&error),
    };
    let brain = app.brain.clone();
    // Spawned before any reply, so a client hanging up can't cut a run short.
    let run = tokio::spawn(async move {
        let _held = permit;
        brain.chat_with(input, PURPOSE).await
    });
    if streaming {
        let events = stream::once(async move {
            match run.await {
                Ok(Ok(result)) => stream_events(result),
                Ok(Err(failure)) => vec![event(&error_body("server_error", &failure.0, None))],
                Err(error) => vec![event(&error_body("server_error", &error.to_string(), None))],
            }
        })
        .flat_map(|events| stream::iter(events.into_iter().map(Ok::<_, Infallible>)));
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
            .into_response()
    } else {
        match run.await {
            Ok(Ok(result)) => whole_reply(result),
            Ok(Err(failure)) => wire_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                &failure.0,
                None,
            ),
            Err(error) => from_api(&ApiError::internal(&error)),
        }
    }
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(extra: &Value) -> OpenAiRequest {
        let mut body = json!({
            "model": MODEL_ID,
            "messages": [{ "role": "user", "content": "hi" }],
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        serde_json::from_value(body).unwrap()
    }

    fn refused(extra: &Value) -> String {
        chat_input(request(extra), 100).unwrap_err()
    }

    #[test]
    fn a_turn_keeps_its_place_and_the_last_is_the_question() {
        let input = chat_input(
            request(&json!({ "messages": [
                { "role": "system", "content": "Be brief." },
                { "role": "user", "content": "one" },
                { "role": "assistant", "content": "two" },
                { "role": "developer", "content": "Be kind." },
                { "role": "user", "content": [
                    { "type": "text", "text": "three" }, { "type": "text", "text": "four" }
                ] },
            ] })),
            100,
        )
        .unwrap();
        assert_eq!(input.extra_system.as_deref(), Some("Be brief.\n\nBe kind."));
        assert_eq!(input.history.len(), 2);
        assert_eq!(input.question, "three\nfour");
    }

    #[test]
    fn a_client_cant_raise_the_servers_output_limit() {
        let limit = |extra: Value| chat_input(request(&extra), 100).unwrap().max_output_tokens;
        assert_eq!(limit(json!({})), 100);
        assert_eq!(limit(json!({ "max_tokens": 5000 })), 100);
        assert_eq!(limit(json!({ "max_tokens": 5 })), 5);
        assert_eq!(
            limit(json!({ "max_tokens": 5, "max_completion_tokens": 7 })),
            7
        );
    }

    #[test]
    fn what_cant_be_honored_is_refused_not_ignored() {
        assert!(refused(&json!({ "model": "gpt-9" })).contains(MODEL_ID));
        assert!(refused(&json!({ "tools": [{ "type": "function" }] })).contains("tools"));
        let json_mode = json!({ "response_format": { "type": "json_object" } });
        assert!(refused(&json_mode).contains("plain text"));
        let last_is_model = json!({ "messages": [
            { "role": "user", "content": "a" }, { "role": "assistant", "content": "b" }
        ] });
        assert!(refused(&last_is_model).contains("last message"));
        let tool_turn = json!({ "messages": [
            { "role": "tool", "content": "a" }, { "role": "user", "content": "b" }
        ] });
        assert!(refused(&tool_turn).contains("tool"));
        let tool_calls = json!({ "messages": [
            { "role": "assistant", "content": null, "tool_calls": [{ "id": "x" }] },
            { "role": "user", "content": "b" }
        ] });
        assert!(refused(&tool_calls).contains("tool calls"));
        let blank = json!({ "messages": [{ "role": "user", "content": "  " }] });
        assert!(refused(&blank).contains("last message"));
    }

    #[test]
    fn harmless_empties_are_accepted() {
        let extra = json!({ "tools": [], "response_format": { "type": "text" }, "top_p": 1 });
        chat_input(request(&extra), 100).unwrap();
    }

    proptest! {
        #[test]
        fn any_conversation_is_turned_into_a_chat_or_refused(
            roles in proptest::collection::vec(0..5_u8, 1..12),
            text in ".{0,40}",
        ) {
            let messages: Vec<Value> = roles.iter().map(|role| {
                let name = ["system", "developer", "user", "assistant", "tool"]
                    .get(usize::from(*role)).copied().unwrap_or("user");
                json!({ "role": name, "content": text })
            }).collect();
            let _ = chat_input(request(&json!({ "messages": messages })), 10);
        }
    }
}
