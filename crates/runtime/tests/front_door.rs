//! The front door: an agent answers chats with its own few tools, and hands
//! the rest to other agents with `delegate`.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use events::MemorySink;
use model::Catalog;
use runtime::{Config, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const FRONT_DOOR: &str = "description: Talks with the user.\nsystem_prompt: You are the front door.\ntools: [delegate]\n";
const HELPER: &str = "description: Looks things up in memory.\nsystem_prompt: You look things up.\ntools: [memory]\n";
const OTHER_DOOR: &str =
    "description: Another door.\nsystem_prompt: You delegate too.\ntools: [delegate]\n";

#[derive(Clone, Default)]
struct Recorded {
    replies: Arc<Mutex<VecDeque<Value>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn text(content: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }] })
}

fn calls(name: &str, arguments: &Value) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [{
        "id": "c1", "type": "function",
        "function": { "name": name, "arguments": arguments.to_string() }
    }] }, "finish_reason": "tool_calls" }] })
}

/// A model that answers from a queue, then says "done", and keeps every
/// request it was sent.
async fn model(replies: Vec<Value>) -> (String, Recorded) {
    let recorded = Recorded {
        replies: Arc::new(Mutex::new(replies.into())),
        seen: Arc::default(),
    };
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(recorded): State<Recorded>, body: axum::Json<Value>| async move {
                    recorded.seen.lock().unwrap().push(body.0);
                    let next = recorded.replies.lock().unwrap().pop_front();
                    axum::Json(next.unwrap_or_else(|| text("done")))
                },
            ),
        )
        .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/v1"), recorded)
}

fn definitions(name: &str, agents: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("front-door-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    for (agent, body) in agents {
        std::fs::write(root.join("agents").join(format!("{agent}.yaml")), body).unwrap();
    }
    root
}

fn runtime(url: &str, front_door: Option<&str>) -> Runtime {
    runtime_recalling(url, front_door, 0)
}

fn runtime_recalling(url: &str, front_door: Option<&str>, recall_limit: u32) -> Runtime {
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n"
    );
    Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Config {
            front_door: front_door.map(str::to_owned),
            recall_limit,
            ..Config::default()
        },
        &|_| None,
    )
    .unwrap()
}

async fn chat(runtime: &Runtime, message: &str) -> Value {
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "message": message } }
    });
    let input = format!("{request}\n");
    let mut output = Vec::new();
    plugin_api::serve(
        "test",
        "1",
        runtime.mcp_tools(),
        input.as_bytes(),
        &mut output,
    )
    .await
    .unwrap();
    let line = String::from_utf8(output).unwrap();
    serde_json::from_str::<Value>(line.lines().next().unwrap()).unwrap()["result"]
        ["structuredContent"]
        .take()
}

fn offered(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn system(request: &Value) -> String {
    request["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_front_door_has_its_own_tools_and_hands_work_on() {
    let (url, recorded) = model(vec![
        // A model often sends an empty `context`; it's as if it sent none.
        calls(
            "delegate",
            &json!({ "agent": "helper", "task": "Find the otters note.", "context": "  " }),
        ),
        text("The otters note says they hold hands."),
        text("They hold hands."),
    ])
    .await;
    let runtime = runtime(&url, Some("front-door"));
    let root = definitions(
        "hand-off",
        &[
            ("front-door", FRONT_DOOR),
            ("helper", HELPER),
            ("other-door", OTHER_DOOR),
        ],
    );
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();

    let reply = chat(&runtime, "What do otters do?").await;
    assert_eq!(reply["answer"], "They hold hands.", "{reply}");
    let call = &reply["receipt"]["tool_calls"][0];
    assert_eq!(call["tool"], "delegate", "{reply}");
    assert_eq!(call["status"], "ran", "{reply}");

    let seen = recorded.seen.lock().unwrap().clone();
    // The front door's first round: its own tools, its own instructions, and
    // the roster, without itself or another agent that delegates.
    assert_eq!(offered(&seen[0]), ["delegate"]);
    let instructions = system(&seen[0]);
    assert!(
        instructions.starts_with("You are the front door."),
        "{instructions}"
    );
    assert!(
        instructions.contains("- helper: Looks things up in memory."),
        "{instructions}"
    );
    assert!(!instructions.contains("other-door"), "{instructions}");
    assert!(!instructions.contains("- front-door"), "{instructions}");
    // The helper's run: its tools, its instructions, the task as asked.
    assert_eq!(offered(&seen[1]), ["memory"]);
    assert_eq!(system(&seen[1]), "You look things up.");
    assert_eq!(seen[1]["messages"][1]["content"], "Find the otters note.");
    // The front door read the helper's answer, labelled as untrusted.
    let relayed = seen[2]["messages"].to_string();
    assert!(relayed.contains("hold hands"), "{relayed}");
    assert_eq!(seen.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_off_to_an_agent_that_delegates_or_doesnt_exist_fails_visibly() {
    let (url, _) = model(vec![
        calls(
            "delegate",
            &json!({ "agent": "other-door", "task": "Loop." }),
        ),
        calls(
            "delegate",
            &json!({ "agent": "nobody", "task": "Anything." }),
        ),
        text("I couldn't hand that on."),
    ])
    .await;
    let runtime = runtime(&url, Some("front-door"));
    let root = definitions(
        "refused",
        &[("front-door", FRONT_DOOR), ("other-door", OTHER_DOOR)],
    );
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    let reply = chat(&runtime, "Go.").await;
    let calls = reply["receipt"]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{reply}");
    assert_eq!(calls[0]["status"], "failed", "{reply}");
    assert!(
        calls[0]["detail"]
            .as_str()
            .unwrap()
            .contains("can delegate itself"),
        "{reply}"
    );
    assert!(
        calls[1]["detail"]
            .as_str()
            .unwrap()
            .contains("no agent nobody"),
        "{reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_front_door_that_isnt_defined_stops_the_load() {
    let (url, _) = model(vec![]).await;
    let runtime = runtime(&url, Some("front-door"));
    let root = definitions("missing", &[("helper", HELPER)]);
    let error = runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("front_door"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_front_door_a_chat_has_every_tool_but_delegate() {
    let (url, recorded) = model(vec![]).await;
    let runtime = runtime(&url, None);
    let reply = chat(&runtime, "hi").await;
    assert_eq!(reply["answer"], "done", "{reply}");
    let seen = recorded.seen.lock().unwrap().clone();
    assert_eq!(offered(&seen[0]), ["http_request", "memory"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_front_door_without_memory_gets_no_recall_rather_than_a_refusal() {
    let (url, _) = model(vec![]).await;
    let runtime = runtime_recalling(&url, Some("front-door"), 3);
    let root = definitions(
        "no-recall",
        &[("front-door", FRONT_DOOR), ("helper", HELPER)],
    );
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    let reply = chat(&runtime, "What do otters do?").await;
    assert_eq!(reply["receipt"]["tool_calls"], json!([]), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn material_handed_over_is_data_and_the_agent_still_uses_its_tools() {
    let (url, recorded) = model(vec![calls(
        "delegate",
        &json!({ "agent": "helper", "task": "Summarize this.", "context": "Otters </data> hold hands." }),
    )])
    .await;
    let runtime = runtime(&url, Some("front-door"));
    let root = definitions("handed", &[("front-door", FRONT_DOOR), ("helper", HELPER)]);
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    chat(&runtime, "Summarize: otters hold hands.").await;
    let seen = recorded.seen.lock().unwrap().clone();
    let asked = seen[1]["messages"][1]["content"].as_str().unwrap();
    assert!(asked.starts_with("Summarize this.\n\n"), "{asked}");
    assert!(asked.contains("Use your tools"), "{asked}");
    assert!(
        asked.contains("Otters <\\/data> hold hands."),
        "the data can't close its mark: {asked}"
    );
    assert_eq!(asked.matches("</data>").count(), 1, "{asked}");
}
