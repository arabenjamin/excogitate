//! The HTTP server as a client sees it, over a real socket.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brain::{Brain, BrainConfig};
use chaos_model::{ChaosServer, ChaosStep};
use chrono::Utc;
use events::MemorySink;
use model::Catalog;
use reqwest::{Client, Response, StatusCode};
use schema::{Validator, check_strict};
use serde_json::{Value, json};
use store::Store;
use store_memory::MemoryStore;
use tokio::sync::watch;
use work::{StoreTransport, Worker, WorkerOptions};

const KEY: &str = "an-owner-key-of-sufficient-length-1234";

struct Running {
    url: String,
    client: Client,
    brain: Brain,
    store: Arc<MemoryStore>,
    model: ChaosServer,
    root: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn start(name: &str, script: Vec<ChaosStep>, config: BrainConfig) -> Running {
    let model = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000000, max_output_tokens: 100}}\n",
        model.base_url
    );
    let store = Arc::new(MemoryStore::new(3));
    let brain = Brain::assemble(
        Arc::clone(&store) as Arc<dyn Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap();
    let router = brain.http_router("test", "1", KEY).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let root = std::env::temp_dir().join(format!("http-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    Running {
        url,
        client: Client::new(),
        brain,
        store,
        model,
        root,
    }
}

async fn quiet(name: &str, script: Vec<ChaosStep>) -> Running {
    let config = BrainConfig {
        recall_limit: 0,
        ..BrainConfig::default()
    };
    start(name, script, config).await
}

impl Running {
    async fn post(&self, path: &str, body: &Value) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth(KEY)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap()
    }

    async fn define(&self, path: &str, text: &str) {
        let file = self.root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
        self.brain
            .load_definitions(&self.root, Utc::now())
            .await
            .unwrap();
    }

    /// Runs a worker until `job` has ended, and returns the job.
    async fn work_off(&self, job: &str) -> Value {
        let store = Arc::clone(&self.store) as Arc<dyn Store>;
        let worker = Arc::new(
            Worker::new(
                Arc::new(StoreTransport(store)),
                self.brain.job_handlers(),
                WorkerOptions {
                    poll: Duration::from_millis(5),
                    ..WorkerOptions::default()
                },
                "test".to_owned(),
            )
            .unwrap(),
        );
        let (stop, shutdown) = watch::channel(false);
        let running = tokio::spawn(worker.run(shutdown));
        let path = format!("/api/v1/jobs/{job}");
        let mut seen = Value::Null;
        for _ in 0..400 {
            seen = body_of(self.get(&path).await).await;
            if seen["status"] == "succeeded" || seen["status"] == "dead" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.send(true).unwrap();
        running.await.unwrap();
        seen
    }
}

fn rpc(id: u32, method: &str, params: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

async fn body_of(response: Response) -> Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

// The key and what can be reached without it.

#[tokio::test]
async fn health_needs_no_key_and_says_nothing_else() {
    let running = quiet("health", vec![]).await;
    let response = running
        .client
        .get(format!("{}/health", running.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(response).await, json!({ "status": "ok" }));
}

#[tokio::test]
async fn nothing_else_can_be_reached_without_the_key() {
    let running = quiet("nokey", vec![]).await;
    for (method, path) in [
        ("POST", "/mcp"),
        ("GET", "/v1/models"),
        ("POST", "/v1/chat/completions"),
        ("GET", "/api/v1/jobs"),
        ("GET", "/api/v1/definitions/workflow"),
        ("POST", "/api/v1/workflows/x/runs"),
        ("GET", "/no/such/path"),
    ] {
        let url = format!("{}{path}", running.url);
        let builder = running.client.request(method.parse().unwrap(), &url);
        let response = builder.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
    }
}

#[tokio::test]
async fn a_wrong_key_is_refused() {
    let running = quiet("wrongkey", vec![]).await;
    let url = format!("{}/v1/models", running.url);
    let almost = format!("{KEY}x");
    for header in [
        format!("Bearer {almost}"),
        format!("Basic {KEY}"),
        KEY.to_owned(),
    ] {
        let response = running
            .client
            .get(&url)
            .header("authorization", header)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = running
        .client
        .get(&url)
        .header("authorization", format!("bearer {KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the scheme's case doesn't matter"
    );
}

#[tokio::test]
async fn a_short_key_is_never_served_with() {
    let running = quiet("short", vec![]).await;
    assert!(running.brain.http_router("test", "1", "too-short").is_err());
}

#[tokio::test]
async fn a_browser_origin_needs_listing_even_with_the_key() {
    let mut config = BrainConfig::default();
    config.server.allowed_origins = vec!["https://mine.example".to_owned()];
    let running = start("origin", vec![], config).await;
    let url = format!("{}/v1/models", running.url);
    let with_origin = |origin: &'static str| {
        running
            .client
            .get(&url)
            .bearer_auth(KEY)
            .header("origin", origin)
    };
    let evil = with_origin("https://evil.example").send().await.unwrap();
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);
    let mine = with_origin("https://mine.example").send().await.unwrap();
    assert_eq!(mine.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let running = quiet("big", vec![]).await;
    let big = json!({ "inputs": "x".repeat(9 * 1024 * 1024) });
    let response = running.post("/api/v1/workflows/x/runs", &big).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_form_post_is_refused() {
    let running = quiet("form", vec![]).await;
    let response = running
        .client
        .post(format!("{}/api/v1/workflows/x/runs", running.url))
        .bearer_auth(KEY)
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// MCP.

#[tokio::test]
async fn mcp_works_over_http() {
    let running = quiet("mcp", vec![ChaosStep::Text("Hello.".to_owned())]).await;
    let hello = running
        .post("/mcp", &rpc(1, "initialize", &json!({})))
        .await;
    assert_eq!(hello.status(), StatusCode::OK);
    assert_eq!(
        body_of(hello).await["result"]["protocolVersion"],
        plugin_api::MCP_VERSION
    );

    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(
        running.post("/mcp", &note).await.status(),
        StatusCode::ACCEPTED
    );

    let listed = body_of(
        running
            .post("/mcp", &rpc(2, "tools/list", &json!({})))
            .await,
    )
    .await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["chat", "http_request", "memory"]);

    let call = json!({ "name": "chat", "arguments": { "message": "hi" } });
    let reply = body_of(running.post("/mcp", &rpc(3, "tools/call", &call)).await).await;
    assert_eq!(reply["result"]["structuredContent"]["answer"], "Hello.");
    assert_eq!(
        reply["result"]["structuredContent"]["receipt"]["outcome"],
        "answered"
    );
}

#[tokio::test]
async fn mcp_arguments_are_checked() {
    let running = quiet("mcpargs", vec![]).await;
    let call = json!({ "name": "chat", "arguments": { "wrong": 1 } });
    let reply = body_of(running.post("/mcp", &rpc(1, "tools/call", &call)).await).await;
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(running.model.served().len(), 0);
}

#[tokio::test]
async fn malformed_mcp_is_refused() {
    let running = quiet("mcpbad", vec![]).await;
    for body in [
        json!([rpc(1, "ping", &json!({}))]),
        json!({ "id": 1, "method": "ping" }),
        json!(3),
    ] {
        let response = running.post("/mcp", &body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body_of(response).await["error"]["code"], -32600);
    }
    let not_json = running
        .client
        .post(format!("{}/mcp", running.url))
        .bearer_auth(KEY)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(not_json.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_of(not_json).await["error"]["code"], -32700);
}

#[tokio::test]
async fn a_protocol_version_it_doesnt_speak_is_refused() {
    let running = quiet("mcpver", vec![]).await;
    let send = |version: &'static str| {
        running
            .client
            .post(format!("{}/mcp", running.url))
            .bearer_auth(KEY)
            .header("mcp-protocol-version", version)
            .header("content-type", "application/json")
            .body(rpc(1, "ping", &json!({})).to_string())
            .send()
    };
    assert_eq!(
        send("1999-01-01").await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(plugin_api::MCP_VERSION).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn mcp_opens_no_stream_and_keeps_no_session() {
    let running = quiet("mcpget", vec![]).await;
    assert_eq!(
        running.get("/mcp").await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

// The bridge for stdio clients.

async fn bridged(url: &str, key: &str, lines: &[Value]) -> Vec<Value> {
    let joined: Vec<String> = lines.iter().map(ToString::to_string).collect();
    let input = joined.join(
        "
",
    ) + "
";
    let mut output = Vec::new();
    brain::relay(url, key, input.as_bytes(), &mut output)
        .await
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn a_stdio_client_can_use_a_running_server() {
    let running = quiet("bridge", vec![ChaosStep::Text("Bridged.".to_owned())]).await;
    let call = json!({ "name": "chat", "arguments": { "message": "hi" } });
    let lines = [
        rpc(1, "initialize", &json!({})),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        rpc(2, "tools/call", &call),
    ];
    let replies = bridged(&format!("{}/", running.url), KEY, &lines).await;
    assert_eq!(
        replies.len(),
        2,
        "a notification gets no reply: {replies:?}"
    );
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(
        replies[1]["result"]["structuredContent"]["answer"],
        "Bridged."
    );
}

#[tokio::test]
async fn a_wrong_key_is_an_error_the_client_can_read() {
    let running = quiet("bridgekey", vec![]).await;
    let replies = bridged(&running.url, "wrong", &[rpc(7, "ping", &json!({}))]).await;
    assert_eq!(replies[0]["id"], 7);
    assert!(
        replies[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("owner key")
    );
}

#[tokio::test]
async fn a_server_thats_down_is_an_error_and_the_session_goes_on() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let lines = [rpc(1, "ping", &json!({})), rpc(2, "ping", &json!({}))];
    let replies = bridged(&url, KEY, &lines).await;
    let ids: Vec<&Value> = replies.iter().map(|reply| &reply["id"]).collect();
    assert_eq!(ids, [&json!(1), &json!(2)]);
    assert!(
        replies[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("can't reach")
    );
}

#[tokio::test]
async fn a_line_that_isnt_json_gets_an_error() {
    let mut output = Vec::new();
    brain::relay("http://127.0.0.1:9", KEY, &b"{nope\n"[..], &mut output)
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(reply["error"]["code"], -32700);
}

// The front door.

fn ask(extra: &Value) -> Value {
    let mut body = json!({
        "model": "brain",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

#[tokio::test]
async fn the_front_door_lists_its_one_model() {
    let running = quiet("models", vec![]).await;
    let listed = body_of(running.get("/v1/models").await).await;
    assert_eq!(listed["data"][0]["id"], "brain");
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_front_door_answers_in_its_clients_format() {
    let running = quiet("door", vec![ChaosStep::Text("Hello.".to_owned())]).await;
    let response = running
        .post("/v1/chat/completions", &ask(&json!({ "top_p": 0.5 })))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply = body_of(response).await;
    assert_eq!(reply["object"], "chat.completion");
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello.");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
    assert_eq!(reply["usage"]["total_tokens"], 20);
    assert_eq!(reply["receipt"]["outcome"], "answered");
}

#[tokio::test]
async fn a_cut_off_answer_says_so() {
    let running = quiet("length", vec![ChaosStep::CutOff("Half an".to_owned())]).await;
    let reply = body_of(running.post("/v1/chat/completions", &ask(&json!({}))).await).await;
    assert_eq!(reply["choices"][0]["finish_reason"], "length");
}

#[tokio::test]
async fn the_front_door_streams_for_clients_that_ask() {
    let running = quiet("sse", vec![ChaosStep::Text("Hello.".to_owned())]).await;
    let response = running
        .post("/v1/chat/completions", &ask(&json!({ "stream": true })))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = response.text().await.unwrap();
    let events: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect();
    assert_eq!(events.last(), Some(&"[DONE]"));
    let first: Value = serde_json::from_str(events[0]).unwrap();
    assert_eq!(first["object"], "chat.completion.chunk");
    assert_eq!(first["choices"][0]["delta"]["content"], "Hello.");
    let last: Value = serde_json::from_str(events[1]).unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert_eq!(last["receipt"]["outcome"], "answered");
}

#[tokio::test]
async fn a_run_that_stops_is_an_error_with_its_receipt() {
    let running = quiet("stopped", vec![ChaosStep::Status(500)]).await;
    let response = running.post("/v1/chat/completions", &ask(&json!({}))).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let reply = body_of(response).await;
    assert_eq!(reply["error"]["type"], "run_stopped");
    assert_eq!(reply["receipt"]["outcome"], "model_failed");
}

#[tokio::test]
async fn a_run_that_stops_while_streaming_says_so_in_the_stream() {
    let running = quiet("sse-stop", vec![ChaosStep::Status(500)]).await;
    let text = running
        .post("/v1/chat/completions", &ask(&json!({ "stream": true })))
        .await
        .text()
        .await
        .unwrap();
    assert!(text.contains("run_stopped"), "{text}");
    assert!(text.contains("[DONE]"), "{text}");
}

#[tokio::test]
async fn the_front_door_refuses_in_its_clients_format() {
    let running = quiet("doorbad", vec![]).await;
    for extra in [
        json!({ "model": "other" }),
        json!({ "messages": [] }),
        json!({ "temperature": 5 }),
        json!({ "n": 2 }),
        json!({ "tools": [{ "type": "function" }] }),
        json!({ "messages": [{ "role": "wizard", "content": "x" }] }),
    ] {
        let response = running.post("/v1/chat/completions", &ask(&extra)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{extra}");
        assert_eq!(
            body_of(response).await["error"]["type"],
            "invalid_request_error"
        );
    }
    assert_eq!(running.model.served().len(), 0);
}

#[tokio::test]
async fn past_the_limit_a_request_is_turned_away() {
    let mut config = BrainConfig {
        recall_limit: 0,
        ..BrainConfig::default()
    };
    config.server.max_concurrent_requests = 1;
    config.http.allowed_ranges = vec!["127.0.0.1/32".to_owned()];
    let running = Arc::new(start("busy", vec![ChaosStep::Text("x".to_owned())], config).await);
    // Hold the only place with a fetch of a port that never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let call =
        json!({ "name": "http_request", "arguments": { "url": format!("http://{address}/") } });
    let holder = {
        let running = Arc::clone(&running);
        tokio::spawn(async move { running.post("/mcp", &rpc(1, "tools/call", &call)).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = running.post("/v1/chat/completions", &ask(&json!({}))).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_of(response).await["error"]["type"], "server_error");
    holder.abort();
    drop(listener);
}

// REST.

const AGENT: &str = "system_prompt: You help.\ntools: []\n";
const WORKFLOW: &str = "inputs:\n  type: object\n  additionalProperties: false\n  required: [subject]\n  properties:\n    subject: {type: string, maxLength: 100}\n\
    steps:\n  - {type: agent, id: think, agent: helper, message: 'About {{inputs.subject}}'}\n\
    output: '{{steps.think.answer}}'\n";

async fn with_workflow(name: &str, script: Vec<ChaosStep>) -> Running {
    let running = quiet(name, script).await;
    running.define("agents/helper.yaml", AGENT).await;
    running.define("workflows/flow.yaml", WORKFLOW).await;
    running
}

fn inputs() -> Value {
    json!({ "inputs": { "subject": "otters" } })
}

#[tokio::test]
async fn a_workflow_started_over_rest_runs_and_reports() {
    let running = with_workflow("run", vec![ChaosStep::Text("Otters!".to_owned())]).await;
    let response = running.post("/api/v1/workflows/flow/runs", &inputs()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job = body_of(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let queued = body_of(running.get(&format!("/api/v1/jobs/{job}")).await).await;
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["requested_by"], "owner");

    let done = running.work_off(&job).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["result"]["output"], "Otters!");
    let schema = schema::schema_of::<workflow::WorkflowRun>();
    Validator::from_schema(&schema)
        .unwrap()
        .check(&done["result"])
        .unwrap();
}

#[tokio::test]
async fn a_failed_run_keeps_its_steps_for_whoever_looks() {
    let running = with_workflow("failed", vec![ChaosStep::Status(500)]).await;
    let started = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
    let done = running.work_off(started["job_id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "dead");
    assert!(
        done["last_error"]
            .as_str()
            .unwrap()
            .starts_with("step think:")
    );
    assert_eq!(done["result"]["outcome"], "failed");
    assert_eq!(done["result"]["steps"][0]["status"], "failed");
    assert_eq!(
        done["result"]["steps"][0]["receipt"]["outcome"],
        "model_failed"
    );
}

#[tokio::test]
async fn the_same_key_starts_one_run() {
    let running = with_workflow("idem", vec![]).await;
    let body = json!({ "inputs": { "subject": "otters" }, "idempotency_key": "once" });
    let first = running.post("/api/v1/workflows/flow/runs", &body).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_of(first).await;
    assert_eq!(first["already_existed"], false);
    let again = running.post("/api/v1/workflows/flow/runs", &body).await;
    assert_eq!(again.status(), StatusCode::OK);
    let again = body_of(again).await;
    assert_eq!(again["already_existed"], true);
    assert_eq!(again["job_id"], first["job_id"]);
    let listed = body_of(running.get("/api/v1/jobs").await).await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn without_a_key_each_request_is_its_own_run() {
    let running = with_workflow("noidem", vec![]).await;
    let first = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
    let second = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
    assert_ne!(first["job_id"], second["job_id"]);
}

#[tokio::test]
async fn a_run_is_checked_before_its_queued() {
    let running = with_workflow("runbad", vec![]).await;
    let unknown = running.post("/api/v1/workflows/nope/runs", &inputs()).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let misfit = json!({ "inputs": { "other": 1 } });
    let response = running.post("/api/v1/workflows/flow/runs", &misfit).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let typo = json!({ "inputs": { "subject": "x" }, "importance": 3 });
    let response = running.post("/api/v1/workflows/flow/runs", &typo).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_ne!(body_of(response).await["problems"], json!([]));
    let listed = body_of(running.get("/api/v1/jobs").await).await;
    assert_eq!(listed["items"], json!([]));
}

#[tokio::test]
async fn definitions_can_be_read_and_say_where_they_came_from() {
    let running = with_workflow("defs", vec![]).await;
    let listed = body_of(running.get("/api/v1/definitions/workflow").await).await;
    assert_eq!(listed["items"][0]["name"], "flow");
    assert_eq!(listed["items"][0]["source"], "file:workflows/flow.yaml");
    let one = body_of(running.get("/api/v1/definitions/agent/helper").await).await;
    assert_eq!(one["body"]["system_prompt"], "You help.");
    assert_eq!(
        running.get("/api/v1/definitions/agent/nope").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        running.get("/api/v1/definitions/wizard").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn jobs_filter_and_page() {
    let running = with_workflow("page", vec![]).await;
    for _ in 0..3 {
        running.post("/api/v1/workflows/flow/runs", &inputs()).await;
    }
    let page = body_of(running.get("/api/v1/jobs?limit=2").await).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["has_more"], true);
    let rest = body_of(running.get("/api/v1/jobs?limit=2&offset=2").await).await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["has_more"], false);
    let dead = body_of(running.get("/api/v1/jobs?status=dead").await).await;
    assert_eq!(dead["items"], json!([]));
    let queued = body_of(running.get("/api/v1/jobs?status=queued").await).await;
    assert_eq!(queued["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn bad_queries_are_refused() {
    let running = quiet("query", vec![]).await;
    for path in [
        "/api/v1/jobs?limit=0",
        "/api/v1/jobs?limit=201",
        "/api/v1/jobs?limit=many",
        "/api/v1/jobs?statuss=dead",
        "/api/v1/jobs?status=sleeping",
        "/api/v1/jobs/not-a-uuid",
    ] {
        assert_eq!(
            running.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let missing = "/api/v1/jobs/00000000-0000-0000-0000-000000000000";
    assert_eq!(running.get(missing).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_error_has_one_shape_everywhere_in_the_api() {
    let running = quiet("errors", vec![]).await;
    let schema = schema::schema_of::<brain::ApiError>();
    assert_eq!(check_strict(&schema), vec![]);
    let validator = Validator::from_schema(&schema).unwrap();
    for path in ["/api/v1/jobs?limit=0", "/nowhere"] {
        validator
            .check(&body_of(running.get(path).await).await)
            .unwrap();
    }
}
