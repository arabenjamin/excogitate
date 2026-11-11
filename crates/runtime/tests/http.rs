//! The HTTP server as a client sees it, over a real socket.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chaos_model::{ChaosServer, ChaosStep};
use chrono::Utc;
use events::MemorySink;
use model::Catalog;
use reqwest::{Client, Response, StatusCode};
use runtime::{Config, Runtime};
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
    runtime: Runtime,
    store: Arc<MemoryStore>,
    model: ChaosServer,
    root: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn start(name: &str, script: Vec<ChaosStep>, config: Config) -> Running {
    let model = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools, investigative], context_tokens: 1000000, max_output_tokens: 4000}}\n",
        model.base_url
    );
    let store = Arc::new(MemoryStore::new(3));
    let root = std::env::temp_dir().join(format!("http-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let runtime = Runtime::assemble(
        Arc::clone(&store) as Arc<dyn Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap()
    .with_definitions(root.clone());
    let router = runtime.http_router("test", "1", KEY).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Running {
        url,
        client: Client::new(),
        runtime,
        store,
        model,
        root,
    }
}

async fn quiet(name: &str, script: Vec<ChaosStep>) -> Running {
    let config = Config {
        recall_limit: 0,
        ..Config::default()
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
        self.runtime
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
                self.runtime.job_handlers(),
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
    assert!(
        running
            .runtime
            .http_router("test", "1", "too-short")
            .is_err()
    );
}

#[tokio::test]
async fn a_browser_origin_needs_listing_even_with_the_key() {
    let mut config = Config::default();
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
    runtime::relay(url, key, input.as_bytes(), &mut output)
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
    runtime::relay("http://127.0.0.1:9", KEY, &b"{nope\n"[..], &mut output)
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(reply["error"]["code"], -32700);
}

// The front door.

fn ask(extra: &Value) -> Value {
    let mut body = json!({
        "model": "test",
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
    assert_eq!(listed["data"][0]["id"], "test");
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
    let mut config = Config {
        recall_limit: 0,
        ..Config::default()
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
    let schema = schema::schema_of::<runtime::ApiError>();
    assert_eq!(check_strict(&schema), vec![]);
    let validator = Validator::from_schema(&schema).unwrap();
    for path in ["/api/v1/jobs?limit=0", "/nowhere"] {
        validator
            .check(&body_of(running.get(path).await).await)
            .unwrap();
    }
}

// Runs and trajectories.

#[tokio::test]
async fn a_chat_leaves_a_trajectory_that_meets_its_schema() {
    let running = quiet("trajectory", vec![ChaosStep::Text("Hello.".to_owned())]).await;
    let reply = body_of(running.post("/v1/chat/completions", &ask(&json!({}))).await).await;
    let run = reply["receipt"]["run"].as_str().unwrap().to_owned();

    let listed = body_of(running.get("/api/v1/runs?agent=chat").await).await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["items"][0]["run"], run.as_str());
    assert_eq!(listed["items"][0]["outcome"], "answered");
    Validator::from_schema(&schema::schema_of::<runtime::RunList>())
        .unwrap()
        .check(&listed)
        .unwrap();

    let trajectory = body_of(running.get(&format!("/api/v1/runs/{run}")).await).await;
    let schema = schema::schema_of::<dispatch::Trajectory>();
    assert_eq!(check_strict(&schema), vec![]);
    Validator::from_schema(&schema)
        .unwrap()
        .check(&trajectory)
        .unwrap();
    assert_eq!(trajectory["agent"], "chat");
    assert_eq!(trajectory["harness"], listed["items"][0]["harness"]);
    assert_eq!(trajectory["calls"], "answered");
    let steps = trajectory["steps"].as_array().unwrap();
    assert_eq!(steps[0]["step"]["type"], "started");
    assert_eq!(steps[1]["step"]["role"], "system");
    assert_eq!(steps[2]["step"]["role"], "user");
    assert_eq!(steps[2]["step"]["content"], "hi");
    assert_eq!(steps[3]["step"]["reply"], "Hello.");
    assert_eq!(steps.last().unwrap()["step"]["type"], "ended");
}

#[tokio::test]
async fn an_agents_harness_id_changes_with_its_instructions_and_only_then() {
    let script = ["One.", "Two.", "Three."]
        .map(|text| ChaosStep::Text(text.to_owned()))
        .to_vec();
    let running = with_workflow("harness", script).await;
    let mut jobs = Vec::new();
    for prompt in ["You help.", "You help.", "You help briefly."] {
        running
            .define(
                "agents/helper.yaml",
                &format!("system_prompt: {prompt}\ntools: []\n"),
            )
            .await;
        let started = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
        let job = started["job_id"].as_str().unwrap().to_owned();
        assert_eq!(running.work_off(&job).await["status"], "succeeded");
        jobs.push(job);
    }
    let listed = body_of(running.get("/api/v1/runs?agent=agent:helper").await).await;
    let harnesses: Vec<&str> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["harness"].as_str().unwrap())
        .collect();
    assert_eq!(harnesses.len(), 3, "{listed}");
    // Newest first: the changed prompt, then the two runs of the first.
    assert_ne!(harnesses[0], harnesses[1]);
    assert_eq!(harnesses[1], harnesses[2]);
    let same = format!("/api/v1/runs?harness={}", harnesses[1]);
    assert_eq!(
        body_of(running.get(&same).await).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn bad_run_queries_are_refused() {
    let running = quiet("run-query", vec![]).await;
    for path in [
        "/api/v1/runs?limit=0",
        "/api/v1/runs?limit=201",
        "/api/v1/runs?agentt=chat",
        "/api/v1/runs?agent=",
        "/api/v1/runs/not-a-uuid",
    ] {
        assert_eq!(
            running.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let missing = "/api/v1/runs/00000000-0000-0000-0000-000000000000";
    assert_eq!(running.get(missing).await.status(), StatusCode::NOT_FOUND);
}

// Harness².

fn judged(value: &Value) -> ChaosStep {
    ChaosStep::Text(value.to_string())
}

#[tokio::test]
async fn an_attempt_started_over_rest_is_kept_and_can_be_read() {
    let script = vec![
        ChaosStep::Text("I don't know.".to_owned()),
        judged(&json!({
            "invariant": { "system_prompt": "You help. Work it out before you answer.",
                           "change": "Work it out.",
                           "cites": [{ "run": "R", "step": 3, "quote": "I don't know" }] },
            "domain": { "system_prompt": "You help.", "change": "Nothing.", "cites": [] }
        })),
        ChaosStep::Text("It is 42.".to_owned()),
        judged(&json!({
            "verdicts": [{ "candidate": "A", "effect": "mitigated",
                           "cite": { "run": "A", "step": 3, "quote": "It is 42." } }],
            "decision": "adopt", "choose": "A", "merged": null, "reason": "A answered."
        })),
    ];
    let running = with_workflow("attempt", script).await;
    let request = json!({ "agent": "helper", "task": "What is six times seven?", "why": null });
    let response = running.post("/api/v1/harness/attempts", &request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job = body_of(response).await["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = running.work_off(&job).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let id = done["result"]["attempt_id"].as_str().unwrap().to_owned();

    let kept = body_of(running.get(&format!("/api/v1/harness/attempts/{id}")).await).await;
    assert_eq!(kept["attempt"]["status"], "supported", "{kept}");
    assert_eq!(kept["attempt"]["chosen"], "A");
    // The script has no answers for the judge's known-answer test, so it fails
    // the test and nothing is adopted.
    assert_eq!(kept["attempt"]["adoption"]["adopted"], false);
    assert!(
        kept["attempt"]["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("known-answer"),
        "{kept}"
    );
    Validator::from_schema(&schema::schema_of::<runtime::AttemptView>())
        .unwrap()
        .check(&kept)
        .unwrap();
    // The probes are runs of their own, kept apart from the agent's.
    let probes = body_of(running.get("/api/v1/runs?agent=probe:helper").await).await;
    assert_eq!(probes["items"].as_array().unwrap().len(), 2, "{probes}");
    let real = body_of(running.get("/api/v1/runs?agent=agent:helper").await).await;
    assert_eq!(real["items"], json!([]));

    let supported = body_of(
        running
            .get("/api/v1/harness/attempts?status=supported")
            .await,
    )
    .await;
    assert_eq!(supported["items"].as_array().unwrap().len(), 1);
    let failed = body_of(running.get("/api/v1/harness/attempts?status=failed").await).await;
    assert_eq!(failed["items"], json!([]));
}

#[tokio::test]
async fn a_bad_attempt_request_is_refused_before_it_queues() {
    let running = with_workflow("attempt-bad", vec![]).await;
    let ghost = json!({ "agent": "ghost", "task": "Anything." });
    assert_eq!(
        running
            .post("/api/v1/harness/attempts", &ghost)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let extra = json!({ "agent": "helper", "task": "Anything.", "adopt": true });
    assert_eq!(
        running
            .post("/api/v1/harness/attempts", &extra)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    for path in [
        "/api/v1/harness/attempts?status=great",
        "/api/v1/harness/attempts?limit=0",
        "/api/v1/harness/attempts/not-a-uuid",
    ] {
        assert_eq!(
            running.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let jobs = body_of(running.get("/api/v1/jobs").await).await;
    assert_eq!(jobs["items"], json!([]));
}

#[test]
fn a_probe_read_that_isnt_a_tool_stops_the_runtime_starting() {
    let config = Config {
        harness: runtime::HarnessConfig {
            probe_reads: vec!["ghost".to_owned()],
            ..runtime::HarnessConfig::default()
        },
        ..Config::default()
    };
    let error = Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml("version: 1\nproviders: []\nmodels: []\n", |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("ghost"), "{error}");
}

// Adoption.

const HELPER_FILE: &str = "# Kept by hand.\nsystem_prompt: You help.\ntools: []\n";
const WORK_IT_OUT: &str = "You help. Work it out before you answer.";

/// One attempt that ends supported, as the scripted model plays it.
fn supported_attempt() -> Vec<ChaosStep> {
    supported_with(WORK_IT_OUT)
}

fn supported_with(prompt: &str) -> Vec<ChaosStep> {
    vec![
        ChaosStep::Text("I don't know.".to_owned()),
        judged(&json!({
            "invariant": { "system_prompt": prompt, "change": "Work it out.",
                           "cites": [{ "run": "R", "step": 3, "quote": "I don't know" }] },
            "domain": { "system_prompt": "You help.", "change": "Nothing.", "cites": [] }
        })),
        ChaosStep::Text("It is 42.".to_owned()),
        judged(&json!({
            "verdicts": [{ "candidate": "A", "effect": "mitigated",
                           "cite": { "run": "A", "step": 3, "quote": "It is 42." } }],
            "decision": "adopt", "choose": "A", "merged": null, "reason": "A answered."
        })),
    ]
}

/// The judge's three known answers, given correctly.
fn known_answers() -> Vec<ChaosStep> {
    let verdict = |effect: &str, quote: &str| {
        json!([{ "candidate": "A", "effect": effect,
                 "cite": { "run": "A", "step": 3, "quote": quote } }])
    };
    let reply = |verdicts: Value, decision: &str, choose: Option<&str>| {
        judged(
            &json!({ "verdicts": verdicts, "decision": decision, "choose": choose,
                        "merged": null, "reason": "As the runs show." }),
        )
    };
    vec![
        reply(
            verdict("mitigated", "Six times seven is 42."),
            "adopt",
            Some("A"),
        ),
        reply(
            verdict("regressed", "the model's reply was empty"),
            "keep",
            None,
        ),
        reply(verdict("unchanged", "Six times seven is 42."), "keep", None),
    ]
}

impl Running {
    async fn improve(&self) -> Value {
        let request = json!({ "agent": "helper", "task": "What is six times seven?" });
        let started = body_of(self.post("/api/v1/harness/attempts", &request).await).await;
        let done = self.work_off(started["job_id"].as_str().unwrap()).await;
        assert_eq!(done["status"], "succeeded", "{done}");
        done["result"]["attempt"].clone()
    }

    fn helper_file(&self) -> String {
        std::fs::read_to_string(self.root.join("agents/helper.yaml")).unwrap()
    }

    async fn helper_prompt(&self) -> Value {
        let found = body_of(self.get("/api/v1/definitions/agent/helper").await).await;
        found["body"]["system_prompt"].clone()
    }
}

async fn adopting(name: &str, script: Vec<ChaosStep>) -> Running {
    let running = quiet(name, script).await;
    running.define("agents/helper.yaml", HELPER_FILE).await;
    running
}

#[tokio::test]
async fn a_supported_edit_is_adopted_logged_and_can_be_reverted() {
    let script = [
        supported_attempt(),
        known_answers(),
        supported_attempt(),
        supported_attempt(),
    ]
    .concat();
    let running = adopting("adopt", script).await;

    let attempt = running.improve().await;
    assert_eq!(attempt["adoption"]["adopted"], true, "{attempt}");
    let change_id = attempt["adoption"]["change_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The file says what happened and how to undo it, and the agent runs with it.
    let written = running.helper_file();
    assert!(written.starts_with("# Changed by Harness²"), "{written}");
    assert!(written.contains(&format!("changes/{change_id}/revert")));
    assert_eq!(running.helper_prompt().await, WORK_IT_OUT);

    let listed = body_of(
        running
            .get("/api/v1/harness/changes?agent=helper&status=live")
            .await,
    )
    .await;
    assert_eq!(listed["items"][0]["id"], change_id.as_str(), "{listed}");
    let change = &listed["items"][0]["change"];
    assert_eq!(change["old_file"], HELPER_FILE);
    assert_eq!(change["judge_model"], "c");
    assert!(change["notice"].as_str().unwrap().contains("no webhook"));
    assert_eq!(change["evidence"][0]["quote"], "It is 42.");
    Validator::from_schema(&schema::schema_of::<runtime::ChangeList>())
        .unwrap()
        .check(&listed)
        .unwrap();

    // The owner reverts it: the file is back exactly, and the agent is held.
    let reverted = running
        .post(
            &format!("/api/v1/harness/changes/{change_id}/revert"),
            &json!({}),
        )
        .await;
    assert_eq!(reverted.status(), StatusCode::OK);
    assert_eq!(body_of(reverted).await["change"]["status"], "reverted");
    assert_eq!(running.helper_file(), HELPER_FILE);
    assert_eq!(running.helper_prompt().await, "You help.");
    let again = running
        .post(
            &format!("/api/v1/harness/changes/{change_id}/revert"),
            &json!({}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    let why = body_of(again).await;
    assert!(
        why["message"]
            .as_str()
            .unwrap()
            .contains("only a live change"),
        "{why}"
    );
    let holds = body_of(running.get("/api/v1/harness/holds").await).await;
    assert_eq!(holds["items"][0]["agent"], "helper", "{holds}");

    // While it's held, the same evidence changes nothing.
    let second = running.improve().await;
    assert_eq!(second["status"], "supported");
    assert_eq!(second["adoption"]["adopted"], false);
    assert!(
        second["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("held")
    );
    assert_eq!(running.helper_file(), HELPER_FILE);

    let released = running
        .client
        .delete(format!("{}/api/v1/harness/holds/helper", running.url))
        .bearer_auth(KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(released.status(), StatusCode::NO_CONTENT);
    let holds = body_of(running.get("/api/v1/harness/holds").await).await;
    assert_eq!(holds["items"], json!([]));

    // Released, the agent can be improved again, but never back to what the
    // owner reverted.
    let third = running.improve().await;
    assert_eq!(third["adoption"]["adopted"], false, "{third}");
    assert!(
        third["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("reverted this harness"),
        "{third}"
    );
}

#[tokio::test]
async fn an_owners_edit_after_a_change_stands_and_holds_the_agent() {
    let script = [supported_attempt(), known_answers(), supported_attempt()].concat();
    let running = adopting("hand-edit", script).await;
    assert_eq!(running.improve().await["adoption"]["adopted"], true);

    let change = body_of(running.get("/api/v1/harness/changes").await).await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mine = "system_prompt: You help, my way.\ntools: []\n";
    running.define("agents/helper.yaml", mine).await;
    // A revert would undo the owner's edit, so it's refused.
    let revert = running
        .post(
            &format!("/api/v1/harness/changes/{change}/revert"),
            &json!({}),
        )
        .await;
    assert_eq!(revert.status(), StatusCode::CONFLICT);
    let why = body_of(revert).await;
    assert!(
        why["message"].as_str().unwrap().contains("edit it by hand"),
        "{why}"
    );
    let second = running.improve().await;
    assert_eq!(second["adoption"]["adopted"], false, "{second}");
    assert!(
        second["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("the owner edited"),
        "{second}"
    );
    assert_eq!(running.helper_file(), mine);
    let replaced = body_of(running.get("/api/v1/harness/changes?status=replaced").await).await;
    assert_eq!(replaced["items"].as_array().unwrap().len(), 1, "{replaced}");
}

#[tokio::test]
async fn adoption_can_be_turned_off() {
    let config = Config {
        recall_limit: 0,
        harness: runtime::HarnessConfig {
            adopt: false,
            ..runtime::HarnessConfig::default()
        },
        ..Config::default()
    };
    let running = start("adopt-off", supported_attempt(), config).await;
    running.define("agents/helper.yaml", HELPER_FILE).await;
    let attempt = running.improve().await;
    assert_eq!(attempt["adoption"]["adopted"], false);
    assert!(
        attempt["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("turned off")
    );
    assert_eq!(running.helper_file(), HELPER_FILE);
}

#[tokio::test]
async fn bad_change_and_hold_requests_are_refused() {
    let running = adopting("change-bad", vec![]).await;
    for path in [
        "/api/v1/harness/changes?status=gone",
        "/api/v1/harness/changes?agent=Not%20A%20Name",
        "/api/v1/harness/changes/not-a-uuid",
    ] {
        assert_eq!(
            running.get(path).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    let missing = "/api/v1/harness/changes/00000000-0000-0000-0000-000000000000/revert";
    assert_eq!(
        running.post(missing, &json!({})).await.status(),
        StatusCode::NOT_FOUND
    );
    let held = running
        .client
        .put(format!("{}/api/v1/harness/holds/helper", running.url))
        .bearer_auth(KEY)
        .header("content-type", "application/json")
        .body(json!({ "reason": "I'm rewriting it." }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    let holds = body_of(running.get("/api/v1/harness/holds").await).await;
    assert_eq!(holds["items"][0]["reason"], "I'm rewriting it.");
}

#[tokio::test]
async fn an_agent_changes_at_most_once_a_day() {
    let script = [
        supported_attempt(),
        known_answers(),
        supported_with("You help. Check your arithmetic."),
    ]
    .concat();
    let running = adopting("daily", script).await;
    assert_eq!(running.improve().await["adoption"]["adopted"], true);
    let second = running.improve().await;
    assert_eq!(second["adoption"]["adopted"], false, "{second}");
    assert!(
        second["adoption"]["reason"]
            .as_str()
            .unwrap()
            .contains("less than 24 hours"),
        "{second}"
    );
}

// Picking what to improve.

impl Running {
    /// Queues a job straight into the store, as a schedule would, and runs it.
    async fn run_job(&self, kind: &str, payload: Value) -> Value {
        let id = store::JobId::generate();
        let job = store::Job {
            id,
            kind: kind.to_owned(),
            payload,
            priority: 0,
            status: store::JobStatus::Queued,
            requested_by: protocol::Actor::new("test"),
            run_at: Utc::now(),
            attempts: 0,
            max_attempts: 1,
            lease: None,
            last_error: None,
            result: None,
        };
        self.store
            .apply(store::UnitOfWork::new().with(store::WriteOp::EnqueueJob(job)))
            .await
            .unwrap();
        self.work_off(&id.to_string()).await
    }
}

#[tokio::test]
async fn an_agent_that_fails_the_same_way_twice_is_picked_once_a_day() {
    let running = with_workflow("pick", vec![ChaosStep::Status(500), ChaosStep::Status(500)]).await;
    for _ in 0..2 {
        let started = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
        assert_eq!(
            running.work_off(started["job_id"].as_str().unwrap()).await["status"],
            "dead"
        );
    }
    let picked = running.run_job("harness_pick", json!({})).await;
    assert_eq!(picked["status"], "succeeded", "{picked}");
    let result = &picked["result"];
    assert_eq!(result["pick"]["request"]["agent"], "helper", "{result}");
    assert_eq!(result["pick"]["failure"], "ended model_failed");
    assert_eq!(result["pick"]["repeats"], 2);
    assert!(
        result["pick"]["request"]["task"]
            .as_str()
            .unwrap()
            .starts_with("About otters")
    );
    Validator::from_schema(&schema::schema_of::<runtime::Picked>())
        .unwrap()
        .check(result)
        .unwrap();
    let job = result["job_id"].as_str().unwrap().to_owned();
    let queued = body_of(running.get(&format!("/api/v1/jobs/{job}")).await).await;
    assert_eq!(queued["kind"], "harness");
    assert_eq!(queued["requested_by"], "harness");

    // Picking again the same day queues nothing new: the worker runs the
    // attempt first, and an agent tried today is left alone.
    let again = running.run_job("harness_pick", json!({})).await;
    assert_eq!(again["result"]["job_id"], Value::Null, "{again}");
    assert!(
        again["result"]["skipped"][0]
            .as_str()
            .unwrap()
            .contains("was tried in the last 24 hours"),
        "{again}"
    );
    let jobs = body_of(running.get("/api/v1/jobs?limit=200").await).await;
    let attempts = jobs["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|job| job["kind"] == "harness")
        .count();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn a_held_agent_is_not_picked() {
    let running = with_workflow(
        "pick-held",
        vec![ChaosStep::Status(500), ChaosStep::Status(500)],
    )
    .await;
    for _ in 0..2 {
        let started = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
        running.work_off(started["job_id"].as_str().unwrap()).await;
    }
    let held = running
        .client
        .put(format!("{}/api/v1/harness/holds/helper", running.url))
        .bearer_auth(KEY)
        .header("content-type", "application/json")
        .body(json!({ "reason": "Leave it." }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    let picked = running.run_job("harness_pick", json!({})).await;
    assert_eq!(picked["result"]["pick"], Value::Null, "{picked}");
    assert!(
        picked["result"]["skipped"][0]
            .as_str()
            .unwrap()
            .contains("helper is held")
    );
}

#[tokio::test]
async fn runs_that_went_well_give_nothing_to_pick() {
    let script = vec![
        ChaosStep::Text("Otters!".to_owned()),
        ChaosStep::Text("Otters!".to_owned()),
    ];
    let running = with_workflow("pick-none", script).await;
    for _ in 0..2 {
        let started = body_of(running.post("/api/v1/workflows/flow/runs", &inputs()).await).await;
        running.work_off(started["job_id"].as_str().unwrap()).await;
    }
    let picked = running.run_job("harness_pick", json!({})).await;
    assert_eq!(picked["result"]["pick"], Value::Null, "{picked}");
    assert_eq!(picked["result"]["job_id"], Value::Null);
}
