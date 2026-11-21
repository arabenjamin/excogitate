//! Remote tools against a scripted MCP server on loopback, through the real
//! dispatcher: listing, both answer styles, sessions, keys, failures, the
//! privacy rule and the kept listing.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use dispatch::{Budget, CallStatus, Effect, RunSubject, Session, SessionConfig, ToolRegistry};
use events::MemorySink;
use mcp_remote::{McpServerConfig, install};
use model::ToolCall;
use protocol::{DataClass, RunId};
use serde_json::{Value, json};
use store::Store;
use store_memory::MemoryStore;
use tools::HttpConfig;

const KEY: &str = "a-key-for-the-scripted-server-1234";

/// How the scripted server behaves, and what it was sent.
#[derive(Debug, Default)]
struct Script {
    /// Answer as an event stream rather than JSON.
    streams: bool,
    /// Hand out a session id, and refuse a call without it.
    sessions: bool,
    /// The session ids it has forgotten.
    forgotten: Vec<String>,
    /// How many sessions it has started.
    started: u32,
    /// Answer every request with this status.
    down: Option<u16>,
    /// The methods it was sent, in order.
    methods: Vec<String>,
    /// The arguments of each tools/call.
    calls: Vec<Value>,
    /// Be a stateless server (2026-07-28 and later) speaking these
    /// revisions: no handshake, and every request must say its revision in
    /// `_meta` and in headers.
    speaks: Option<Vec<&'static str>>,
}

/// A stateless server's answer to `message`, as a modern one refuses what
/// it can't take: a 400 with the error.
fn stateless_answer(script: &mut Script, headers: &HeaderMap, message: &Value) -> Response {
    let speaks = script.speaks.clone().unwrap_or_default();
    let id = message["id"].clone();
    let method = message["method"].as_str().unwrap_or("").to_owned();
    let refuse = |error: Value| {
        let body = json!({"jsonrpc": "2.0", "id": id, "error": error});
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    };
    let Some(version) = message["params"]["_meta"][plugin_api::VERSION_META].as_str() else {
        return refuse(json!({"code": -32602, "message": "no revision in _meta"}));
    };
    if !speaks.contains(&version) {
        return refuse(
            json!({"code": -32022, "message": "Unsupported protocol version",
                             "data": {"supported": speaks, "requested": version}}),
        );
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if header("mcp-protocol-version") != Some(version) || header("mcp-method") != Some(&method) {
        return refuse(json!({"code": -32020, "message": "headers don't match"}));
    }
    let mut result = match method.as_str() {
        "server/discover" => json!({"supportedVersions": speaks, "capabilities": {"tools": {}},
                                    "ttlMs": 0, "cacheScope": "public"}),
        "tools/list" => json!({"tools": tools(), "ttlMs": 0, "cacheScope": "public"}),
        "tools/call" => {
            // As GitHub's server does: an argument the schema marks
            // x-mcp-header must come in its Mcp-Param header too.
            let query = &message["params"]["arguments"]["query"];
            if message["params"]["name"] == "search-jobs"
                && query.is_string()
                && header("mcp-param-query") != query.as_str()
            {
                return refuse(json!({"code": -32020,
                    "message": "header mismatch: missing Mcp-Param-Query header for parameter \"query\""}));
            }
            script.calls.push(message["params"].clone());
            json!({"content": [{"type": "text", "text": "2 jobs"}],
                   "structuredContent": {"jobs": ["a", "b"]}})
        }
        _ => {
            let body = json!({"jsonrpc": "2.0", "id": id,
                              "error": {"code": -32601, "message": "no such method"}});
            return (StatusCode::NOT_FOUND, Json(body)).into_response();
        }
    };
    result["resultType"] = json!("complete");
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

type Shared = Arc<Mutex<Script>>;

fn tools() -> Value {
    json!([
        {"name": "search-jobs", "description": "Finds jobs.",
         "inputSchema": {"type": "object", "properties": {
             "query": {"type": "string", "x-mcp-header": "Query"}, "limit": {"type": "integer"}},
             "required": ["query"]}},
        {"name": "apply", "description": "Applies.",
         "inputSchema": {"type": "object", "properties": {"job": {"type": "string"}}}},
        {"name": "fail", "inputSchema": {"type": "object"}}
    ])
}

async fn serve(
    State(script): State<Shared>,
    headers: HeaderMap,
    Json(message): Json<Value>,
) -> Response {
    let mut script = script.lock().unwrap();
    if let Some(status) = script.down {
        return (StatusCode::from_u16(status).unwrap(), "down").into_response();
    }
    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(KEY) {
        return (StatusCode::UNAUTHORIZED, format!("no key, and not {KEY}")).into_response();
    }
    let method = message["method"].as_str().unwrap_or("").to_owned();
    script.methods.push(method.clone());
    if script.speaks.is_some() {
        return stateless_answer(&mut script, &headers, &message);
    }
    let mut session = None;
    if method == "initialize" {
        script.started += 1;
        if script.sessions {
            session = Some(format!("s{}", script.started));
        }
    } else if script.sessions {
        let given = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if given.is_empty() {
            return (StatusCode::BAD_REQUEST, "no session").into_response();
        }
        if script.forgotten.iter().any(|old| old == given) {
            return (StatusCode::NOT_FOUND, "no such session").into_response();
        }
    }
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method.as_str() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            script.calls.push(message["params"].clone());
            match message["params"]["name"].as_str() {
                Some("fail") => json!({"isError": true, "content": [{"type": "text", "text": format!("bad job; key {KEY}")}]}),
                _ => json!({"content": [{"type": "text", "text": "2 jobs"}],
                            "structuredContent": {"jobs": ["a", "b"]}}),
            }
        }
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}})).into_response(),
    };
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let mut response = if script.streams {
        let ping = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
        let body = format!("event: message\ndata: {ping}\n\nevent: message\ndata: {reply}\n\n");
        ([("content-type", "text/event-stream")], body).into_response()
    } else {
        Json(reply).into_response()
    };
    if let Some(session) = session {
        response
            .headers_mut()
            .insert("mcp-session-id", session.parse().unwrap());
    }
    response
}

async fn server(script: &Shared) -> String {
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(serve))
        .with_state(Arc::clone(script));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn config(url: &str) -> McpServerConfig {
    serde_json::from_value(json!({
        "name": "jobs", "url": url, "header": "x-api-key", "env": "JOBS_KEY",
        "allow_insecure": true, "no_log": true,
        "tools": ["search-jobs", "fail"], "read_only": ["search-jobs"]
    }))
    .unwrap()
}

fn http() -> HttpConfig {
    HttpConfig {
        allowed_ranges: vec!["127.0.0.0/8".to_owned()],
        ..HttpConfig::default()
    }
}

fn secret(name: &str) -> Option<String> {
    (name == "JOBS_KEY").then(|| KEY.to_owned())
}

async fn installed(
    store: &Arc<MemoryStore>,
    servers: &[McpServerConfig],
) -> Result<(ToolRegistry, Vec<mcp_remote::Installed>), String> {
    let mut registry = ToolRegistry::new();
    let done = install(
        &mut registry,
        Arc::clone(store) as Arc<dyn Store>,
        &http(),
        servers,
        &secret,
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok((registry, done))
}

fn session(registry: ToolRegistry, data_class: DataClass) -> Session {
    let offered: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    Session::new(
        Arc::new(registry),
        Arc::new(MemorySink::new()),
        SessionConfig::new(
            RunId::generate(),
            RunSubject::new("test".to_owned(), "test".to_owned()),
            Budget {
                max_rounds: 3,
                max_tool_calls: 20,
                max_writes: 10,
                max_total_tokens: 10_000,
                max_usd_micros: 0,
                max_request_bytes: 100_000,
                max_duration_ms: 20_000,
            },
        )
        .offering(offered)
        .with_data_class(data_class)
        .with_purpose("test".to_owned())
        .with_max_result_chars(20_000),
    )
    .unwrap()
}

async fn call(session: &mut Session, name: &str, arguments: &Value) -> (CallStatus, String) {
    let done = session
        .dispatch(&ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        })
        .await;
    (done.status, done.content)
}

#[tokio::test]
async fn the_tools_named_are_offered_and_called() {
    for streams in [false, true] {
        let script = Shared::default();
        script.lock().unwrap().streams = streams;
        let url = server(&script).await;
        let store = Arc::new(MemoryStore::new(3));
        let (registry, done) = installed(&store, &[config(&url)]).await.unwrap();
        assert_eq!(done[0].tools, ["jobs_search_jobs", "jobs_fail"]);
        assert_eq!(done[0].from_kept, None);

        let definitions = registry.definitions();
        let search = definitions
            .iter()
            .find(|d| d.name == "jobs_search_jobs")
            .unwrap();
        assert_eq!(search.effect, Effect::Read);
        assert!(
            search
                .description
                .starts_with("Finds jobs. (From the MCP server jobs.)")
        );
        assert_eq!(
            search.input_schema["properties"]["query"]["maxLength"],
            20_000
        );
        let fail = definitions.iter().find(|d| d.name == "jobs_fail").unwrap();
        assert_eq!(fail.effect, Effect::Write);
        assert!(
            definitions.iter().all(|d| d.name != "jobs_apply"),
            "only what's named"
        );

        let mut session = session(registry, DataClass::Private);
        let (status, content) = call(
            &mut session,
            "jobs_search_jobs",
            &json!({"query": "rust", "limit": 5}),
        )
        .await;
        assert_eq!(status, CallStatus::Ran, "{content}");
        assert!(
            content.contains("2 jobs") && content.contains("\"jobs\":[\"a\",\"b\"]"),
            "{content}"
        );
        let script = script.lock().unwrap();
        assert_eq!(
            script.calls[0],
            json!({"name": "search-jobs", "arguments": {"query": "rust", "limit": 5}})
        );
        assert_eq!(
            script.methods,
            [
                "server/discover",
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ],
            "one probe, then one handshake, reused"
        );
    }
}

#[tokio::test]
async fn arguments_are_checked_before_anything_is_sent() {
    let script = Shared::default();
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let (registry, _) = installed(&store, &[config(&url)]).await.unwrap();
    let mut session = session(registry, DataClass::Private);
    let (status, _) = call(
        &mut session,
        "jobs_search_jobs",
        &json!({"query": "x", "rm": "-rf"}),
    )
    .await;
    assert_eq!(status, CallStatus::Refused);
    let (status, _) = call(
        &mut session,
        "jobs_search_jobs",
        &json!({"query": "x".repeat(20_001)}),
    )
    .await;
    assert_eq!(status, CallStatus::Refused);
    assert_eq!(script.lock().unwrap().calls.len(), 0);
}

#[tokio::test]
async fn a_failure_says_why_without_the_key() {
    let script = Shared::default();
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let (registry, _) = installed(&store, &[config(&url)]).await.unwrap();
    let mut session = session(registry, DataClass::Private);
    let (status, content) = call(&mut session, "jobs_fail", &json!({})).await;
    assert_eq!(status, CallStatus::Failed, "{content}");
    assert!(
        content.contains("the MCP server jobs: bad job"),
        "{content}"
    );
    assert!(!content.contains(KEY), "{content}");
}

#[tokio::test]
async fn a_private_run_reaches_only_a_no_log_server() {
    let script = Shared::default();
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let mut logging = config(&url);
    logging.no_log = false;
    let (registry, _) = installed(&store, &[logging.clone()]).await.unwrap();
    let mut private = session(registry, DataClass::Private);
    let (status, content) = call(&mut private, "jobs_search_jobs", &json!({"query": "x"})).await;
    assert_eq!(status, CallStatus::Failed, "{content}");
    assert!(
        content.contains("the run is private") && content.contains("isn't marked no_log"),
        "{content}"
    );
    assert_eq!(script.lock().unwrap().calls.len(), 0);

    let (registry, _) = installed(&store, &[logging]).await.unwrap();
    let mut public = session(registry, DataClass::Public);
    let (status, content) = call(&mut public, "jobs_search_jobs", &json!({"query": "x"})).await;
    assert_eq!(status, CallStatus::Ran, "{content}");
}

#[tokio::test]
async fn a_stateless_server_is_spoken_to_without_a_handshake() {
    let script = Shared::default();
    script.lock().unwrap().speaks = Some(vec![plugin_api::STATELESS_VERSION]);
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let (registry, done) = installed(&store, &[config(&url)]).await.unwrap();
    assert_eq!(done[0].tools, ["jobs_search_jobs", "jobs_fail"]);
    let mut session = session(registry, DataClass::Private);
    let (status, content) = call(&mut session, "jobs_search_jobs", &json!({"query": "rust"})).await;
    assert_eq!(status, CallStatus::Ran, "{content}");
    let script = script.lock().unwrap();
    assert_eq!(
        script.methods,
        ["server/discover", "tools/list", "tools/call"],
        "one probe, and no handshake"
    );
    let sent = &script.calls[0];
    assert_eq!(sent["name"], "search-jobs");
    assert_eq!(sent["arguments"]["query"], "rust");
    assert_eq!(
        sent["_meta"][plugin_api::VERSION_META],
        plugin_api::STATELESS_VERSION
    );
    assert!(sent["_meta"][plugin_api::CAPABILITIES_META].is_object());
}

/// The scripted server refuses a search-jobs call whose Mcp-Param-Query
/// header doesn't match, as GitHub's does: the call above ran, so it came.
#[tokio::test]
async fn a_marked_argument_goes_in_its_header() {
    let script = Shared::default();
    script.lock().unwrap().speaks = Some(vec![plugin_api::STATELESS_VERSION]);
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let (registry, _) = installed(&store, &[config(&url)]).await.unwrap();
    let mut session = session(registry, DataClass::Private);
    for query in ["rust", "two words"] {
        let (status, content) =
            call(&mut session, "jobs_search_jobs", &json!({"query": query})).await;
        assert_eq!(status, CallStatus::Ran, "{query}: {content}");
    }
    assert_eq!(script.lock().unwrap().calls.len(), 2);
}

#[tokio::test]
async fn a_server_that_speaks_only_a_newer_revision_says_so() {
    let script = Shared::default();
    script.lock().unwrap().speaks = Some(vec!["2099-01-01"]);
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let error = installed(&store, &[config(&url)]).await.unwrap_err();
    assert!(error.contains("2099-01-01"), "{error}");
    assert!(
        !script
            .lock()
            .unwrap()
            .methods
            .contains(&"initialize".to_owned()),
        "a modern refusal is never taken for an old server"
    );
}

#[tokio::test]
async fn a_forgotten_session_is_started_again_once() {
    let script = Shared::default();
    script.lock().unwrap().sessions = true;
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    let (registry, _) = installed(&store, &[config(&url)]).await.unwrap();
    let mut session = session(registry, DataClass::Private);
    script.lock().unwrap().forgotten.push("s1".to_owned());
    let (status, content) = call(&mut session, "jobs_search_jobs", &json!({"query": "x"})).await;
    assert_eq!(status, CallStatus::Ran, "{content}");
    assert_eq!(script.lock().unwrap().started, 2);
}

#[tokio::test]
async fn a_server_down_at_startup_uses_its_kept_listing() {
    let script = Shared::default();
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));
    installed(&store, &[config(&url)]).await.unwrap();

    script.lock().unwrap().down = Some(503);
    let (registry, done) = installed(&store, &[config(&url)]).await.unwrap();
    let kept = done[0].from_kept.clone().unwrap();
    assert!(
        kept.contains("503") && kept.contains("using its listing of"),
        "{kept}"
    );
    let mut session = session(registry, DataClass::Private);
    let (status, content) = call(&mut session, "jobs_search_jobs", &json!({"query": "x"})).await;
    assert_eq!(status, CallStatus::Failed);
    assert!(
        content.contains("the MCP server jobs: it answered 503"),
        "{content}"
    );

    // Back up: the same tools answer, with no restart.
    script.lock().unwrap().down = None;
    let (status, content) = call(&mut session, "jobs_search_jobs", &json!({"query": "x"})).await;
    assert_eq!(status, CallStatus::Ran, "{content}");
}

#[tokio::test]
async fn what_cant_be_set_up_stops_startup_and_says_why() {
    let script = Shared::default();
    let url = server(&script).await;
    let store = Arc::new(MemoryStore::new(3));

    let mut unknown = config(&url);
    unknown.tools.push("delete-everything".to_owned());
    let error = installed(&store, &[unknown]).await.unwrap_err();
    assert!(
        error.contains("doesn't offer delete-everything; it offers apply, fail, search-jobs"),
        "{error}"
    );

    script.lock().unwrap().down = Some(503);
    let fresh = Arc::new(MemoryStore::new(3));
    let error = installed(&fresh, &[config(&url)]).await.unwrap_err();
    assert!(error.contains("no listing of it is kept"), "{error}");

    let mut blocked = HttpConfig::default();
    blocked.allowed_ranges.clear();
    let mut registry = ToolRegistry::new();
    let error = install(
        &mut registry,
        fresh as Arc<dyn Store>,
        &blocked,
        &[config(&url)],
        &secret,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("that address is blocked"), "{error}");

    let mut keyless = config(&url);
    keyless.env = Some("NOT_SET".to_owned());
    let error = installed(&store, &[keyless]).await.unwrap_err();
    assert!(error.contains("NOT_SET isn't set"), "{error}");
}
