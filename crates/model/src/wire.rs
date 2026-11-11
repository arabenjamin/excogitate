//! The `OpenAI` wire format. Requests are built here, and replies are read
//! tolerantly: servers add fields, and only the ones used are looked at.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    Capability, ChatRequest, ChatResponse, FinishReason, Message, Model, ModelError, Role,
    ToolCall, Usage,
};

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn message_to_wire(message: &Message) -> Value {
    let mut wire = Map::new();
    wire.insert("role".to_owned(), json!(role_name(message.role)));
    wire.insert("content".to_owned(), json!(message.content));
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                })
            })
            .collect();
        wire.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    if let Some(id) = &message.tool_call_id {
        wire.insert("tool_call_id".to_owned(), json!(id));
    }
    Value::Object(wire)
}

/// The JSON body of a chat call. A model the server can't constrain is told
/// the schema in a message instead.
pub(crate) fn chat_body(model: &Model, request: &ChatRequest, output_tokens: u32) -> Value {
    let mut messages: Vec<Value> = request.messages.iter().map(message_to_wire).collect();
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model.wire_model));
    body.insert("max_tokens".to_owned(), json!(output_tokens));
    body.insert("stream".to_owned(), json!(false));
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(wanted) = &request.response_schema {
        if model.can(Capability::JsonSchema) {
            let format = json!({
                "type": "json_schema",
                "json_schema": { "name": wanted.name, "schema": wanted.schema },
            });
            body.insert("response_format".to_owned(), format);
        } else {
            messages.push(json!({
                "role": "system",
                "content": format!(
                    "Reply with one JSON object and nothing else. It must meet this JSON Schema:\n{}",
                    wanted.schema
                ),
            }));
        }
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    Value::Object(body)
}

/// The JSON body of an embeddings call.
pub(crate) fn embeddings_body(model: &Model, inputs: &[String]) -> Value {
    json!({ "model": model.wire_model, "input": inputs })
}

#[derive(Deserialize)]
struct WireChat {
    #[serde(default)]
    choices: Vec<WireChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireMessage {
    content: Option<Value>,
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: Option<String>,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    /// What a router billed, in dollars, if it says.
    cost: Option<f64>,
}

/// A reported cost in dollars as millionths of a dollar, rounded up. Anything
/// that isn't a sensible amount is ignored.
fn reported_micros(dollars: Option<f64>) -> Option<u64> {
    let micros = (dollars? * 1_000_000.0).ceil();
    // u64::MAX as f64 rounds up, so stop well short of it.
    (0.0..1e18).contains(&micros).then(|| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked above to be a whole number in range"
        )]
        let whole = micros as u64;
        whole
    })
}

/// Content is a string on most servers, and a list of text parts on some.
fn content_text(content: Option<Value>) -> String {
    match content {
        Some(Value::String(text)) => text,
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// Arguments are a JSON string by the spec, and an object on some servers.
fn arguments_text(arguments: Option<Value>) -> String {
    match arguments {
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn finish_reason(name: Option<&str>) -> FinishReason {
    match name {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        _ => FinishReason::Other,
    }
}

/// Reads a chat reply.
///
/// # Errors
///
/// [`ModelError::BadResponse`] if it isn't one.
pub fn parse_chat(body: &[u8]) -> Result<ChatResponse, ModelError> {
    let wire: WireChat = serde_json::from_slice(body)
        .map_err(|error| ModelError::BadResponse(format!("not a chat reply: {error}")))?;
    let usage = wire.usage.and_then(|usage| {
        Some(Usage {
            input_tokens: usage.prompt_tokens?,
            output_tokens: usage.completion_tokens?,
            reported_cost_usd_micros: reported_micros(usage.cost),
        })
    });
    let Some(choice) = wire.choices.into_iter().next() else {
        return Err(ModelError::BadResponse(
            "the reply has no choices".to_owned(),
        ));
    };
    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: call.id.unwrap_or_else(|| format!("call_{index}")),
            name: call.function.name,
            arguments: arguments_text(call.function.arguments),
        })
        .collect();
    Ok(ChatResponse {
        content: content_text(choice.message.content),
        tool_calls,
        finish: finish_reason(choice.finish_reason.as_deref()),
        usage,
    })
}

#[derive(Deserialize)]
struct WireEmbeddings {
    #[serde(default)]
    data: Vec<WireEmbedding>,
}

#[derive(Deserialize)]
struct WireEmbedding {
    index: Option<usize>,
    embedding: Vec<f32>,
}

/// Reads an embeddings reply, in input order: one vector per input, each of
/// the expected width.
pub(crate) fn parse_embeddings(
    body: &[u8],
    count: usize,
    width: usize,
) -> Result<Vec<Vec<f32>>, ModelError> {
    let wire: WireEmbeddings = serde_json::from_slice(body)
        .map_err(|error| ModelError::BadResponse(format!("not an embeddings reply: {error}")))?;
    if wire.data.len() != count {
        return Err(ModelError::BadResponse(format!(
            "asked for {count} embeddings and got {}",
            wire.data.len()
        )));
    }
    let mut ordered: Vec<Option<Vec<f32>>> = vec![None; count];
    for (position, item) in wire.data.into_iter().enumerate() {
        if item.embedding.len() != width {
            return Err(ModelError::BadResponse(format!(
                "embedding has width {}, expected {width}",
                item.embedding.len()
            )));
        }
        let slot = ordered
            .get_mut(item.index.unwrap_or(position))
            .ok_or_else(|| ModelError::BadResponse("embedding index out of range".to_owned()))?;
        if slot.replace(item.embedding).is_some() {
            return Err(ModelError::BadResponse(
                "embedding index repeats".to_owned(),
            ));
        }
    }
    ordered
        .into_iter()
        .map(|vector| vector.ok_or_else(|| ModelError::BadResponse("embedding missing".to_owned())))
        .collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ResponseSchema;
    use crate::catalog::test_support::catalog;
    use proptest::prelude::*;
    use std::sync::Arc;

    fn model(capabilities: &str) -> Arc<Model> {
        let yaml = format!(
            "version: 1\nproviders:\n  - {{name: p, base_url: 'http://h/v1', no_log: true, timeout_secs: 5}}\n\
             models:\n  - {{name: m, provider: p, wire_model: wire-id, capabilities: [{capabilities}], context_tokens: 9, max_output_tokens: 9}}\n"
        );
        Arc::clone(&catalog(&yaml).models()[0])
    }

    fn request(schema: bool) -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_output_tokens: 5,
            temperature: None,
            response_schema: schema.then(|| ResponseSchema {
                name: "answer".to_owned(),
                schema: json!({ "type": "object" }),
            }),
        }
    }

    #[test]
    fn a_body_uses_the_wire_id_and_the_clamped_limit() {
        let body = chat_body(&model("chat"), &request(false), 3);
        assert_eq!(body["model"], "wire-id");
        assert_eq!(body["max_tokens"], 3);
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn a_capable_model_gets_response_format() {
        let body = chat_body(&model("chat, json_schema"), &request(true), 3);
        assert_eq!(body["response_format"]["json_schema"]["name"], "answer");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn another_model_is_told_the_schema_in_a_message() {
        let body = chat_body(&model("chat"), &request(true), 3);
        assert!(body.get("response_format").is_none());
        let told = body["messages"][1]["content"].as_str().unwrap();
        assert!(told.contains("JSON Schema"));
    }

    #[test]
    fn tool_messages_keep_their_links() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls.push(ToolCall {
            id: "c1".to_owned(),
            name: "t".to_owned(),
            arguments: "{}".to_owned(),
        });
        let wire = message_to_wire(&assistant);
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "t");
        assert_eq!(
            message_to_wire(&Message::tool("c1", "r"))["tool_call_id"],
            "c1"
        );
    }

    #[test]
    fn extra_fields_are_ignored() {
        let body = br#"{"id":"x","weird":1,"choices":[{"index":0,"extra":[],
            "message":{"role":"assistant","content":"hello","reasoning_content":"mm"},
            "finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":4,"timings":{}}}"#;
        let reply = parse_chat(body).unwrap();
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.finish, FinishReason::Stop);
        assert_eq!(
            reply.usage,
            Some(Usage {
                input_tokens: 3,
                output_tokens: 4,
                reported_cost_usd_micros: None,
            })
        );
    }

    #[test]
    fn a_billed_cost_is_read_and_nonsense_is_ignored() {
        let billed = |cost: &str| {
            let body = format!(
                r#"{{"choices":[{{"message":{{"content":"a"}}}}],
                "usage":{{"prompt_tokens":3,"completion_tokens":4,"cost":{cost}}}}}"#
            );
            parse_chat(body.as_bytes())
                .unwrap()
                .usage
                .unwrap()
                .reported_cost_usd_micros
        };
        assert_eq!(billed("0.000955"), Some(955));
        assert_eq!(billed("0.0000001"), Some(1));
        assert_eq!(billed("0"), Some(0));
        assert_eq!(billed("-1"), None);
        assert_eq!(billed("1e300"), None);
        assert_eq!(billed("null"), None);
    }

    #[test]
    fn content_parts_and_null_content_are_read() {
        let parts =
            br#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"text":"b"}]}}]}"#;
        assert_eq!(parse_chat(parts).unwrap().content, "ab");
        let null = br#"{"choices":[{"message":{"content":null}}]}"#;
        let reply = parse_chat(null).unwrap();
        assert_eq!((reply.content.as_str(), reply.usage), ("", None));
        assert_eq!(reply.finish, FinishReason::Other);
    }

    #[test]
    fn tool_call_arguments_are_kept_raw_however_they_came() {
        let body = br#"{"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[
            {"id":"a","function":{"name":"one","arguments":"{\"x\":"}},
            {"function":{"name":"two","arguments":{"y":1}}}]}}]}"#;
        let reply = parse_chat(body).unwrap();
        assert_eq!(reply.tool_calls[0].arguments, "{\"x\":");
        assert_eq!(reply.tool_calls[1].id, "call_1");
        assert_eq!(reply.tool_calls[1].arguments, "{\"y\":1}");
    }

    #[test]
    fn no_choices_is_a_bad_reply() {
        assert!(matches!(
            parse_chat(br#"{"choices":[]}"#),
            Err(ModelError::BadResponse(_))
        ));
    }

    #[test]
    fn embeddings_come_back_in_input_order() {
        let body = br#"{"data":[{"index":1,"embedding":[3,4]},{"index":0,"embedding":[1,2]}]}"#;
        assert_eq!(
            parse_embeddings(body, 2, 2).unwrap(),
            vec![vec![1.0, 2.0], vec![3.0, 4.0]]
        );
    }

    #[test]
    fn embeddings_of_the_wrong_shape_are_refused() {
        let wrong_count = br#"{"data":[{"embedding":[1,2]}]}"#;
        assert!(parse_embeddings(wrong_count, 2, 2).is_err());
        assert!(parse_embeddings(wrong_count, 1, 3).is_err());
        let repeated = br#"{"data":[{"index":0,"embedding":[1]},{"index":0,"embedding":[2]}]}"#;
        assert!(parse_embeddings(repeated, 2, 1).is_err());
        let out_of_range = br#"{"data":[{"index":9,"embedding":[1]}]}"#;
        assert!(parse_embeddings(out_of_range, 1, 1).is_err());
    }

    proptest! {
        #[test]
        fn no_bytes_panic_the_chat_parser(body in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_chat(&body);
        }

        #[test]
        fn no_bytes_panic_the_embeddings_parser(body in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_embeddings(&body, 2, 2);
        }
    }
}
