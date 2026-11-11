//! A model server that misbehaves on purpose. It plays a seeded script of
//! malformed JSON, wrong types, extra fields, huge strings, made-up tools and
//! obeyed prompt injections. The same seed always gives the same script.
//!
//! It obeys injections deliberately: the harness passes when obeying does no
//! harm, not when the injection is spotted.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Text a page can carry to make this model call a tool:
/// `<<call NAME ARGS>>`.
const INJECTION_OPEN: &str = "<<call ";
const INJECTION_CLOSE: &str = ">>";

/// What the script knows about a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChaosTool {
    /// The tool's name.
    pub name: String,
    /// Arguments that meet its schema.
    pub good_arguments: Value,
}

/// One reply from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChaosStep {
    /// A normal text reply.
    Text(String),
    /// A text reply stopped by the output limit.
    CutOff(String),
    /// One or more tool calls, as `(name, raw arguments)`.
    ToolCalls(Vec<(String, String)>),
    /// Does what the latest tool result's injection says, if it has one.
    ObeyInjection,
    /// A reply claiming absurd token usage.
    LyingUsage,
    /// A body that isn't JSON.
    MalformedBody,
    /// JSON of the wrong shape.
    WrongShape,
    /// No choices.
    EmptyChoices,
    /// An HTTP error.
    Status(u16),
}

impl ChaosStep {
    /// Whether this step leaves the caller with no usable reply.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::MalformedBody | Self::WrongShape | Self::EmptyChoices | Self::Status(_)
        )
    }
}

/// A small generator with a stable output, so a seed means the same script
/// on every version of every dependency.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound.max(1) as u64).unwrap_or(0)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len()))
    }
}

/// Makes up a call that's wrong in one of the ways models are wrong.
fn hostile_call(rng: &mut SplitMix, tools: &[ChaosTool]) -> (String, String) {
    let tool = rng.pick(tools);
    let name = tool.map_or_else(|| "nothing".to_owned(), |tool| tool.name.clone());
    let good = tool.map_or_else(|| json!({}), |tool| tool.good_arguments.clone());
    let mut with_extra = good.clone();
    if let Some(fields) = with_extra.as_object_mut() {
        // The classic: a model trying to mark private data public.
        fields.insert("data_class".to_owned(), json!("public"));
    }
    let mut wrong_types = good.clone();
    if let Some(fields) = wrong_types.as_object_mut() {
        for value in fields.values_mut() {
            *value = json!(12345);
        }
    }
    let mut huge = good.clone();
    if let Some(fields) = huge.as_object_mut() {
        for value in fields.values_mut() {
            *value = json!("A".repeat(300_000));
        }
    }
    match rng.below(9) {
        0 | 1 => (name, good.to_string()),
        2 => (name, with_extra.to_string()),
        3 => (name, wrong_types.to_string()),
        4 => (name, huge.to_string()),
        5 => (name, "{\"unterminated\": ".to_owned()),
        6 => ("delete_everything".to_owned(), "{}".to_owned()),
        7 => ("x".repeat(5000), good.to_string()),
        _ => (name, "null".to_owned()),
    }
}

/// A seeded list of replies.
///
/// `tools` are what the model is offered; `forbidden` names real tools it
/// wasn't, which the script calls anyway.
#[must_use]
pub fn generate_script(
    seed: u64,
    length: usize,
    tools: &[ChaosTool],
    forbidden: &[String],
) -> Vec<ChaosStep> {
    let mut rng = SplitMix(seed);
    (0..length)
        .map(|_| match rng.below(20) {
            0..=8 => {
                let count = 1 + rng.below(3);
                ChaosStep::ToolCalls((0..count).map(|_| hostile_call(&mut rng, tools)).collect())
            }
            9 | 10 => {
                let name = rng
                    .pick(forbidden)
                    .cloned()
                    .unwrap_or_else(|| "forbidden".to_owned());
                ChaosStep::ToolCalls(vec![(name, "{\"text\":\"hi\"}".to_owned())])
            }
            11 => ChaosStep::ToolCalls(
                (0..600)
                    .map(|n| (format!("storm_{n}"), "{}".to_owned()))
                    .collect(),
            ),
            12..=14 => ChaosStep::ObeyInjection,
            15 => ChaosStep::LyingUsage,
            16 => ChaosStep::MalformedBody,
            17 => ChaosStep::WrongShape,
            18 => ChaosStep::EmptyChoices,
            _ => ChaosStep::Status(if rng.below(2) == 0 { 429 } else { 500 }),
        })
        .collect()
}

#[derive(Debug)]
struct Shared {
    script: Mutex<VecDeque<ChaosStep>>,
    served: Mutex<Vec<ChaosStep>>,
}

/// A running server.
#[derive(Debug)]
pub struct ChaosServer {
    /// The URL to configure as a provider's `base_url`.
    pub base_url: String,
    shared: Arc<Shared>,
}

impl ChaosServer {
    /// Starts serving `script` on a local port. After the script, every
    /// request gets a plain final answer.
    ///
    /// # Errors
    ///
    /// The bind error, if no port can be had.
    pub async fn start(script: Vec<ChaosStep>) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", script).await
    }

    /// Starts serving `script` on the address given.
    ///
    /// # Errors
    ///
    /// The bind error, if the address can't be had.
    pub async fn start_on(address: &str, script: Vec<ChaosStep>) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            script: Mutex::new(script.into()),
            served: Mutex::default(),
        });
        let router = Router::new()
            .route("/v1/chat/completions", post(answer))
            .with_state(Arc::clone(&shared));
        let listener = TcpListener::bind(address).await?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        tokio::spawn(async move {
            // The server lives as long as the test; a failure shows as
            // transport errors on the client.
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { base_url, shared })
    }

    /// The steps served so far, in order.
    #[must_use]
    pub fn served(&self) -> Vec<ChaosStep> {
        self.shared
            .served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn chat_reply(message: &Value, usage: &Value) -> Response {
    finished_reply(message, usage, "stop")
}

fn finished_reply(message: &Value, usage: &Value, finish: &str) -> Response {
    let body = json!({
        "id": "chaos",
        "unexpected_field": { "ignored": true },
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage,
    });
    body.to_string().into_response()
}

fn tool_call_message(calls: &[(String, String)]) -> Value {
    let wire: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            json!({
                "id": format!("chaos_{index}"),
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": wire })
}

/// The injected call in the latest tool result, if there is one.
fn injected_call(request: &Value) -> Option<(String, String)> {
    let messages = request.get("messages")?.as_array()?;
    let text = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))?
        .get("content")?
        .as_str()?;
    let (_, after) = text.split_once(INJECTION_OPEN)?;
    let (inside, _) = after.split_once(INJECTION_CLOSE)?;
    let (name, arguments) = inside.split_once(' ')?;
    // The injection sits inside a JSON string, so its quotes are escaped.
    Some((name.to_owned(), arguments.replace("\\\"", "\"")))
}

async fn answer(State(shared): State<Arc<Shared>>, body: String) -> Response {
    let step = shared
        .script
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop_front();
    let Some(step) = step else {
        return chat_reply(
            &json!({ "role": "assistant", "content": "done" }),
            &json!({ "prompt_tokens": 1, "completion_tokens": 1 }),
        );
    };
    shared
        .served
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(step.clone());
    let usage = json!({ "prompt_tokens": 10, "completion_tokens": 10 });
    match step {
        ChaosStep::Text(text) => {
            chat_reply(&json!({ "role": "assistant", "content": text }), &usage)
        }
        ChaosStep::CutOff(text) => finished_reply(
            &json!({ "role": "assistant", "content": text }),
            &usage,
            "length",
        ),
        ChaosStep::ToolCalls(calls) => chat_reply(&tool_call_message(&calls), &usage),
        ChaosStep::ObeyInjection => {
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match injected_call(&request) {
                Some(call) => chat_reply(&tool_call_message(&[call]), &usage),
                None => chat_reply(
                    &json!({ "role": "assistant", "content": "nothing to obey" }),
                    &usage,
                ),
            }
        }
        ChaosStep::LyingUsage => chat_reply(
            &json!({ "role": "assistant", "content": "cheap, I promise" }),
            &json!({ "prompt_tokens": u32::MAX, "completion_tokens": u32::MAX }),
        ),
        ChaosStep::MalformedBody => "{\"choices\": [ not json".into_response(),
        ChaosStep::WrongShape => json!({ "choices": "many" }).to_string().into_response(),
        ChaosStep::EmptyChoices => json!({ "choices": [] }).to_string().into_response(),
        ChaosStep::Status(code) => StatusCode::from_u16(code)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ChaosTool> {
        vec![ChaosTool {
            name: "reader".to_owned(),
            good_arguments: json!({ "text": "hi" }),
        }]
    }

    #[test]
    fn the_same_seed_gives_the_same_script() {
        let a = generate_script(7, 50, &tools(), &[]);
        assert_eq!(a, generate_script(7, 50, &tools(), &[]));
        assert_ne!(a, generate_script(8, 50, &tools(), &[]));
    }

    #[test]
    fn a_long_script_uses_every_kind_of_misbehavior() {
        let script = generate_script(1, 2000, &tools(), &["hidden".to_owned()]);
        assert!(script.iter().any(|s| matches!(s, ChaosStep::ObeyInjection)));
        assert!(script.iter().any(|s| matches!(s, ChaosStep::LyingUsage)));
        assert!(script.iter().any(ChaosStep::is_fatal));
        let calls: Vec<_> = script
            .iter()
            .filter_map(|s| match s {
                ChaosStep::ToolCalls(calls) => Some(calls),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(calls.iter().any(|(_, a)| a.contains("data_class")));
        assert!(calls.iter().any(|(_, a)| a.len() > 100_000));
        assert!(calls.iter().any(|(n, _)| n == "delete_everything"));
        assert!(calls.iter().any(|(n, _)| n == "hidden"));
    }

    #[test]
    fn an_injection_is_read_from_the_latest_tool_result() {
        let request = json!({ "messages": [
            { "role": "tool", "content": "<<call old {}>>" },
            { "role": "user", "content": "<<call user {}>>" },
            { "role": "tool", "content": "page text <<call evil {\\\"a\\\":1}>> more" },
        ]});
        assert_eq!(
            injected_call(&request),
            Some(("evil".to_owned(), "{\"a\":1}".to_owned()))
        );
        assert_eq!(injected_call(&json!({ "messages": [] })), None);
    }
}
