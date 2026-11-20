//! People, keys, roles and scopes, as a client sees them over a real socket.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use events::MemorySink;
use model::Catalog;
use reqwest::{Client, Method, StatusCode};
use runtime::{Config, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const OWNER_KEY: &str = "an-owner-key-of-sufficient-length-1234";

struct Server {
    url: String,
    client: Client,
}

async fn serve() -> Server {
    // Nothing here calls a model, so the provider is never reached.
    let yaml = "version: 1\nproviders:\n  - {name: c, base_url: 'http://127.0.0.1:9', no_log: true, timeout_secs: 1}\n\
                models:\n  - {name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}\n";
    let runtime = Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(yaml, |_| None).unwrap(),
        Config::default(),
        &|_| None,
    )
    .unwrap();
    let router = runtime.http_router("test", "1", OWNER_KEY).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server {
        url,
        client: Client::new(),
    }
}

impl Server {
    async fn call(
        &self,
        method: Method,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(key);
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn person(&self, slug: &str, role: &str) -> StatusCode {
        let body = json!({ "name": slug, "role": role });
        let path = format!("/api/v1/people/{slug}");
        self.call(Method::PUT, &path, OWNER_KEY, Some(body)).await.0
    }

    async fn key(&self, person: &str, scopes: &[&str]) -> (String, String) {
        let body = json!({ "person": person, "scopes": scopes, "name": "test" });
        let (status, made) = self
            .call(Method::POST, "/api/v1/keys", OWNER_KEY, Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        (
            made["key"]["id"].as_str().unwrap().to_owned(),
            made["secret"].as_str().unwrap().to_owned(),
        )
    }

    async fn mcp(&self, key: &str, method: &str, params: Value) -> Value {
        let message = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        self.call(Method::POST, "/mcp", key, Some(message)).await.1
    }
}

#[tokio::test]
async fn a_persons_key_does_what_its_scopes_say_and_no_more() {
    let server = serve().await;
    assert_eq!(server.person("sam", "member").await, StatusCode::CREATED);
    assert_eq!(server.person("sam", "member").await, StatusCode::OK);
    let (_, reader) = server.key("person:sam", &["read"]).await;

    let (status, me) = server.call(Method::GET, "/api/v1/me", &reader, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["actor"], "person:sam");
    assert_eq!(me["role"], "member");
    assert_eq!(me["scopes"], json!(["read"]));

    let (status, _) = server
        .call(Method::GET, "/api/v1/jobs", &reader, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let run = json!({ "inputs": {} });
    let (status, refused) = server
        .call(Method::POST, "/api/v1/workflows/w/runs", &reader, Some(run))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        refused["message"].as_str().unwrap().contains("run scope"),
        "{refused}"
    );

    // People and keys are the owner key's alone.
    for (method, path) in [
        (Method::GET, "/api/v1/people"),
        (Method::GET, "/api/v1/keys"),
    ] {
        let (status, _) = server.call(method, path, &reader, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let (status, own) = server
        .call(Method::GET, "/api/v1/me/keys", &reader, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own["items"].as_array().unwrap().len(), 1);
    assert!(own.to_string().find("digest").is_none());
    for filter in ["", "sam", "person:", "person:Sam"] {
        let path = format!("/api/v1/keys?person={filter}");
        let (status, _) = server.call(Method::GET, &path, OWNER_KEY, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filter:?}");
    }
}

#[tokio::test]
async fn mcp_lists_only_what_the_key_may_call_and_refuses_the_rest() {
    let server = serve().await;
    server.person("sam", "member").await;
    let (_, reader) = server.key("person:sam", &["read"]).await;

    let everything = server.mcp(OWNER_KEY, "tools/list", json!({})).await;
    let listed = server.mcp(&reader, "tools/list", json!({})).await;
    let names = |reply: &Value| -> Vec<String> {
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(names(&everything).contains(&"chat".to_owned()));
    assert!(!names(&listed).contains(&"chat".to_owned()), "{listed}");
    assert!(names(&listed).contains(&"calculate".to_owned()), "{listed}");
    // Memory both reads and writes, so a reading key sees it, and may search.
    assert!(names(&listed).contains(&"memory".to_owned()), "{listed}");
    let search = json!({ "name": "memory", "arguments": { "operation": "search", "query": "x" } });
    let searched = server.mcp(&reader, "tools/call", search).await;
    assert!(searched.get("error").is_none(), "{searched}");
    let store = json!({ "name": "memory", "arguments": { "operation": "store", "text": "c" } });
    let refused_store = server.mcp(&reader, "tools/call", store).await;
    assert_eq!(refused_store["error"]["code"], -32001, "{refused_store}");

    let chat = json!({ "name": "chat", "arguments": { "message": "hi" } });
    let refused = server.mcp(&reader, "tools/call", chat).await;
    assert_eq!(refused["error"]["code"], -32001, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("chat scope")
    );

    let sum = json!({ "name": "calculate", "arguments": { "expression": "1 + 2" } });
    let ran = server.mcp(&reader, "tools/call", sum).await;
    assert_eq!(ran["result"]["isError"], false, "{ran}");

    // The REST route to a tool is held to the same rule.
    let (status, _) = server
        .call(
            Method::POST,
            "/api/v1/tools/chat",
            &reader,
            Some(json!({ "message": "hi" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_revoked_key_or_a_lowered_role_stops_at_once() {
    let server = serve().await;
    server.person("ada", "admin").await;
    let (id, admin) = server.key("person:ada", &["read", "admin"]).await;
    let (status, _) = server
        .call(Method::GET, "/api/v1/harness/holds", &admin, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // A viewer may not hold a key with the admin scope.
    server.person("vic", "viewer").await;
    let body = json!({ "person": "person:vic", "scopes": ["admin"], "name": "x" });
    let (status, refused) = server
        .call(Method::POST, "/api/v1/keys", OWNER_KEY, Some(body))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(refused["message"].as_str().unwrap().contains("admin scope"));

    server.person("ada", "viewer").await;
    let (status, refused) = server
        .call(
            Method::DELETE,
            "/api/v1/harness/holds/someone",
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        refused["message"].as_str().unwrap().contains("viewer"),
        "{refused}"
    );

    let path = format!("/api/v1/me/keys/{id}");
    let (status, _) = server.call(Method::DELETE, &path, &admin, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, refused) = server.call(Method::GET, "/api/v1/me", &admin, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused["message"], "this key was revoked");
}

#[tokio::test]
async fn a_made_up_key_is_refused_without_saying_why() {
    let server = serve().await;
    let made_up = format!("exk_{}_{}", "0".repeat(32), "0".repeat(64));
    for key in [made_up.as_str(), "not-a-key"] {
        let (status, refused) = server.call(Method::GET, "/api/v1/me", key, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(refused["message"].as_str().unwrap().contains("bearer"));
    }
    let (status, owner) = server
        .call(Method::GET, "/api/v1/me", OWNER_KEY, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(owner["role"], "owner");
    assert_eq!(owner["key"], Value::Null);
}

#[tokio::test]
async fn a_run_names_who_asked_for_it() {
    let server = serve().await;
    server.person("sam", "member").await;
    let (_, reader) = server.key("person:sam", &["read"]).await;
    let sum = json!({ "name": "calculate", "arguments": { "expression": "2 * 3" } });
    let ran = server.mcp(&reader, "tools/call", sum).await;
    assert_eq!(ran["result"]["isError"], false, "{ran}");
    let (status, _) = server
        .call(
            Method::POST,
            "/api/v1/tools/calculate",
            OWNER_KEY,
            Some(json!({ "expression": "1 + 1" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, sams) = server
        .call(
            Method::GET,
            "/api/v1/runs?requested_by=person:sam",
            &reader,
            None,
        )
        .await;
    let items = sams["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{sams}");
    assert_eq!(items[0]["agent"], "tool:calculate");
    assert_eq!(items[0]["requested_by"], "person:sam");
    let (_, owners) = server
        .call(
            Method::GET,
            "/api/v1/runs?requested_by=owner",
            OWNER_KEY,
            None,
        )
        .await;
    assert_eq!(owners["items"].as_array().unwrap().len(), 1, "{owners}");

    let path = format!("/api/v1/runs/{}", items[0]["run"].as_str().unwrap());
    let (_, trajectory) = server.call(Method::GET, &path, &reader, None).await;
    assert_eq!(trajectory["requested_by"], "person:sam");
    assert_eq!(trajectory["steps"][0]["step"]["requested_by"], "person:sam");
}

#[tokio::test]
async fn a_private_run_is_seen_by_whoever_asked_and_the_owner_only() {
    let server = serve().await;
    server.person("sam", "member").await;
    server.person("ada", "member").await;
    let body = json!({ "person": "person:sam", "scopes": ["read"], "name": "own",
                       "private_by_default": true });
    let (status, made) = server
        .call(Method::POST, "/api/v1/keys", OWNER_KEY, Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["key"]["private_by_default"], true);
    let sam = made["secret"].as_str().unwrap().to_owned();
    let (_, ada) = server.key("person:ada", &["read"]).await;

    let sum = json!({ "name": "calculate", "arguments": { "expression": "2 * 3" } });
    let ran = server.mcp(&sam, "tools/call", sum).await;
    assert_eq!(ran["result"]["isError"], false, "{ran}");

    let (_, mine) = server.call(Method::GET, "/api/v1/runs", &sam, None).await;
    assert_eq!(mine["items"].as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(mine["items"][0]["private"], true);
    let run = mine["items"][0]["run"].as_str().unwrap().to_owned();

    let (_, theirs) = server.call(Method::GET, "/api/v1/runs", &ada, None).await;
    assert_eq!(theirs["items"], json!([]), "{theirs}");
    assert_eq!(theirs["hidden"], 1);
    let path = format!("/api/v1/runs/{run}");
    let (status, _) = server.call(Method::GET, &path, &ada, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, trajectory) = server.call(Method::GET, &path, &sam, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trajectory["private"], true);

    let (_, everything) = server
        .call(Method::GET, "/api/v1/runs", OWNER_KEY, None)
        .await;
    assert_eq!(everything["items"].as_array().unwrap().len(), 1);
    assert_eq!(everything["hidden"], 0);
}
