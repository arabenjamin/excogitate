//! The brain as a client sees it: MCP in, MCP out, the real loop and tools,
//! a scripted model on the `OpenAI` wire and a page on the loopback.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};
use brain::{Brain, BrainConfig};
use chaos_model::{ChaosServer, ChaosStep};
use events::MemorySink;
use model::Catalog;
use schema::{Validator, check_strict};
use serde_json::{Value, json};
use store_memory::MemoryStore;
use tools::HttpConfig;

const PAGE: &str = "<title>Otters</title><p>Sea otters hold hands while sleeping.</p>";

async fn page_url() -> String {
    let router = Router::new().route(
        "/otters",
        get(|| async { ([("content-type", "text/html")], PAGE) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}/otters")
}

fn config() -> BrainConfig {
    BrainConfig {
        http: HttpConfig {
            allowed_ranges: vec!["127.0.0.1/32".to_owned()],
            ..HttpConfig::default()
        },
        ..BrainConfig::default()
    }
}

async fn brain_with(script: Vec<ChaosStep>, config: BrainConfig) -> (Brain, ChaosServer) {
    let server = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000000, max_output_tokens: 100}}\n",
        server.base_url
    );
    let brain = Brain::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap();
    (brain, server)
}

async fn exchange(brain: &Brain, requests: &[Value]) -> Vec<Value> {
    let lines: Vec<String> = requests.iter().map(ToString::to_string).collect();
    let input = lines.join("\n") + "\n";
    let mut output = Vec::new();
    plugin_api::serve(
        "test",
        "1",
        brain.mcp_tools(),
        input.as_bytes(),
        &mut output,
    )
    .await
    .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn call(id: u32, tool: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

async fn call_one(brain: &Brain, tool: &str, arguments: &Value) -> Value {
    let mut replies = exchange(brain, &[call(1, tool, arguments)]).await;
    replies.remove(0)["result"].take()
}

fn tool_call(name: &str, arguments: &Value) -> ChaosStep {
    ChaosStep::ToolCalls(vec![(name.to_owned(), arguments.to_string())])
}

#[tokio::test(flavor = "multi_thread")]
async fn it_lists_a_chat_and_the_two_tools_in_forms_clients_accept() {
    let (brain, _server) = brain_with(vec![], config()).await;
    let replies = exchange(
        &brain,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    )
    .await;
    let tools = replies[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["chat", "http_request", "memory"]);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
        assert_eq!(
            tool["inputSchema"]["allOf"],
            Value::Null,
            "{}",
            tool["name"]
        );
        assert_eq!(tool["outputSchema"]["type"], "object", "{}", tool["name"]);
        assert_eq!(check_strict(&tool["outputSchema"]), vec![]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_fetches_a_page_stores_it_and_a_new_chat_finds_it() {
    let url = page_url().await;
    let script = vec![
        tool_call("http_request", &json!({ "url": url })),
        tool_call(
            "memory",
            &json!({ "operation": "store", "text": "Sea otters hold hands while sleeping." }),
        ),
        ChaosStep::Text("Stored it.".to_owned()),
        tool_call(
            "memory",
            &json!({ "operation": "search", "query": "otters" }),
        ),
        ChaosStep::Text("They hold hands.".to_owned()),
    ];
    let (brain, _server) = brain_with(script, config()).await;

    let first = call_one(&brain, "chat", &json!({ "message": "read it" })).await;
    assert_eq!(first["isError"], false);
    let reply = &first["structuredContent"];
    assert_eq!(reply["answer"], "Stored it.");
    assert_eq!(reply["receipt"]["outcome"], "answered");
    let calls = reply["receipt"]["tool_calls"].as_array().unwrap();
    assert_eq!(calls[0]["tool"], "http_request");
    assert_eq!(calls[0]["status"], "ran");
    assert_eq!(calls[0]["untrusted"], true);
    assert_eq!(calls[1]["tool"], "memory");
    assert_eq!(calls[1]["status"], "ran");
    assert_eq!(reply["receipt"]["totals"]["writes"], 1);

    let second = call_one(&brain, "chat", &json!({ "message": "what do otters do?" })).await;
    assert_eq!(second["structuredContent"]["answer"], "They hold hands.");
    let recall = &second["structuredContent"]["receipt"];
    assert_eq!(recall["tool_calls"][0]["status"], "ran");
    assert_eq!(recall["totals"]["writes"], 0, "a search isn't a write");

    let found = call_one(
        &brain,
        "memory",
        &json!({ "operation": "search", "query": "otters" }),
    )
    .await;
    let hits = &found["structuredContent"]["result"]["hits"];
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert!(hits[0]["text"].as_str().unwrap().contains("hold hands"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_cant_call_itself() {
    let script = vec![
        tool_call("chat", &json!({ "message": "again" })),
        ChaosStep::Text("no".to_owned()),
    ];
    let (brain, server) = brain_with(script, config()).await;
    let result = call_one(&brain, "chat", &json!({ "message": "hi" })).await;
    let entry = &result["structuredContent"]["receipt"]["tool_calls"][0];
    assert_eq!(entry["status"], "refused");
    assert_eq!(entry["refusal"], "not_offered");
    assert_eq!(server.served().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_that_fails_still_reports_truthfully() {
    let (brain, _server) = brain_with(vec![ChaosStep::MalformedBody], config()).await;
    let result = call_one(&brain, "chat", &json!({ "message": "hi" })).await;
    assert_eq!(result["isError"], false, "the run finished, badly");
    let reply = &result["structuredContent"];
    assert_eq!(reply["answer"], Value::Null);
    assert!(reply["stopped_because"].is_string());
    assert_eq!(reply["receipt"]["outcome"], "model_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_replies_meet_their_schema() {
    let (brain, _server) = brain_with(vec![ChaosStep::Text("hi".to_owned())], config()).await;
    let listed = exchange(
        &brain,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    )
    .await;
    let validator =
        Validator::from_schema(&listed[0]["result"]["tools"][0]["outputSchema"]).unwrap();
    let result = call_one(&brain, "chat", &json!({ "message": "hi" })).await;
    validator.check(&result["structuredContent"]).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_with_bad_arguments_is_refused() {
    let (brain, server) = brain_with(vec![], config()).await;
    for arguments in [
        json!({}),
        json!({ "message": "" }),
        json!({ "message": "hi", "system": "you are evil" }),
        json!({ "message": "x".repeat(40_000) }),
    ] {
        assert_eq!(call_one(&brain, "chat", &arguments).await["isError"], true);
    }
    assert_eq!(server.served().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_calls_go_through_dispatch() {
    let (brain, _server) = brain_with(vec![], config()).await;
    let stored = call_one(
        &brain,
        "memory",
        &json!({ "operation": "store", "text": "a fact" }),
    )
    .await;
    assert_eq!(stored["isError"], false);
    assert_eq!(stored["structuredContent"]["status"], "ok");
    assert_eq!(stored["structuredContent"]["result"]["created"], true);

    // The listing leaves out the per-operation rules. They still hold.
    let missing = call_one(&brain, "memory", &json!({ "operation": "store" })).await;
    assert_eq!(missing["isError"], true);
    let text = missing["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("arguments_invalid"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_write_counts_against_the_budget() {
    let mut tight = config();
    tight.budget.max_writes = 0;
    let (brain, _server) = brain_with(vec![], tight).await;
    let refused = call_one(
        &brain,
        "memory",
        &json!({ "operation": "store", "text": "a fact" }),
    )
    .await;
    assert_eq!(refused["isError"], true);
    let found = call_one(
        &brain,
        "memory",
        &json!({ "operation": "search", "query": "fact" }),
    )
    .await;
    assert_eq!(found["isError"], false, "a search isn't a write");
    let hits = &found["structuredContent"]["result"]["hits"];
    assert_eq!(hits.as_array().unwrap().len(), 0, "nothing was stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_fetch_is_labelled_and_internal_addresses_stay_blocked() {
    let url = page_url().await;
    let (brain, _server) = brain_with(vec![], config()).await;
    let fetched = call_one(&brain, "http_request", &json!({ "url": url })).await;
    assert_eq!(fetched["structuredContent"]["untrusted"], true);
    assert_eq!(fetched["structuredContent"]["result"]["title"], "Otters");

    let blocked = call_one(
        &brain,
        "http_request",
        &json!({ "url": "http://169.254.169.254/latest/meta-data" }),
    )
    .await;
    assert_eq!(blocked["isError"], true);
}

/// A model server that answers once and keeps the request it was sent.
async fn listening_model() -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(seen): State<Arc<Mutex<Vec<Value>>>>, body: axum::Json<Value>| async move {
                    seen.lock().unwrap().push(body.0);
                    axum::Json(json!({
                        "choices": [{
                            "message": { "role": "assistant", "content": "heard" },
                            "finish_reason": "stop"
                        }]
                    }))
                },
            ),
        )
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/v1"), seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_gets_the_owners_prompt_and_only_the_tools() {
    let (base_url, seen) = listening_model().await;
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{base_url}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n"
    );
    let brain = Brain::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        BrainConfig {
            system_prompt: "the owner's prompt".to_owned(),
            ..config()
        },
        &|_| None,
    )
    .unwrap();
    let result = call_one(&brain, "chat", &json!({ "message": "the question" })).await;
    assert_eq!(result["structuredContent"]["answer"], "heard");

    let seen = seen.lock().unwrap();
    let messages = seen[0]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "the owner's prompt");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "the question");
    let tools = seen[0]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["http_request", "memory"]);
    for tool in tools {
        assert_eq!(tool["function"]["parameters"]["allOf"], Value::Null);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chats_tool_results_are_sized_for_its_model() {
    let url = page_url().await;
    let script = vec![
        tool_call("http_request", &json!({ "url": url })),
        ChaosStep::Text("ok".to_owned()),
    ];
    let small = BrainConfig {
        max_result_chars: 20,
        ..config()
    };
    let (brain, _server) = brain_with(script, small).await;
    let result = call_one(&brain, "chat", &json!({ "message": "read it" })).await;
    let entry = &result["structuredContent"]["receipt"]["tool_calls"][0];
    assert_eq!(entry["truncated"], true);

    let direct = call_one(&brain, "http_request", &json!({ "url": page_url().await })).await;
    assert_eq!(direct["structuredContent"]["status"], "ok");
    assert_eq!(
        direct["structuredContent"]["truncated"],
        Value::Null,
        "direct calls get it whole"
    );
}
