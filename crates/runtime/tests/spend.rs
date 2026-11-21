//! A person's spend on priced models, held to their own day cap inside the
//! providers', as a client sees it over a real socket.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use chaos_model::{ChaosServer, ChaosStep};
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
    _model: ChaosServer,
}

/// A server whose writer needs the one priced model, where a person without
/// a cap of their own gets `default_cap`.
async fn serve(default_cap: u64) -> Server {
    let model = ChaosServer::start(vec![ChaosStep::Text("Hello.".to_owned()); 8])
        .await
        .unwrap();
    let url = &model.base_url;
    let yaml = format!(
        "version: 1\nproviders:\n\
         \x20 - {{name: free, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         \x20 - {{name: paid, base_url: '{url}', no_log: true, timeout_secs: 10, \
         spend_cap_usd_micros_per_day: 1000000, paid_for: [chat]}}\n\
         models:\n\
         \x20 - {{name: cheap, provider: free, wire_model: 'm1', capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n\
         \x20 - {{name: capable, provider: paid, wire_model: 'm2', capabilities: [chat, tools, investigative], context_tokens: 100000, \
         max_output_tokens: 100, price: {{input_usd_micros_per_mtok: 1000, output_usd_micros_per_mtok: 1000}}}}\n"
    );
    let mut config = Config::default();
    config.budget.max_usd_micros = 100_000;
    config.people.default_day_cap_usd_micros = default_cap;
    let runtime = Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap();
    let root = std::env::temp_dir().join(format!("spend-{}", uuid::Uuid::new_v4()));
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let writer =
        "description: Talks.\nsystem_prompt: You talk.\ntools: []\nneeds: [investigative]\n";
    std::fs::write(root.join("agents/writer.yaml"), writer).unwrap();
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let router = runtime.http_router("test", "1", OWNER_KEY).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server {
        url: address,
        client: Client::new(),
        _model: model,
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

    async fn person(&self, slug: &str, cap: Option<u64>) -> StatusCode {
        let mut body = json!({ "name": slug, "role": "member" });
        if let Some(cap) = cap {
            body["day_cap_usd_micros"] = json!(cap);
        }
        let path = format!("/api/v1/people/{slug}");
        self.call(Method::PUT, &path, OWNER_KEY, Some(body)).await.0
    }

    async fn key(&self, person: &str) -> String {
        let body = json!({ "person": person, "scopes": ["chat", "read"], "name": "test" });
        let (status, made) = self
            .call(Method::POST, "/api/v1/keys", OWNER_KEY, Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made["secret"].as_str().unwrap().to_owned()
    }

    /// A chat over MCP: what it said, and whether it failed.
    async fn chat(&self, key: &str) -> Value {
        let message = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "chat", "arguments": { "message": "hello" } }
        });
        self.call(Method::POST, "/mcp", key, Some(message)).await.1["result"].take()
    }

    async fn me(&self, key: &str) -> Value {
        let (status, me) = self.call(Method::GET, "/api/v1/me", key, None).await;
        assert_eq!(status, StatusCode::OK, "{me}");
        me
    }
}

#[tokio::test]
async fn a_person_without_a_cap_cant_use_a_priced_model() {
    let server = serve(0).await;
    server.person("sam", None).await;
    let sam = server.key("person:sam").await;
    let me = server.me(&sam).await;
    assert_eq!(me["spend"]["day_cap_usd_micros"], 0, "{me}");
    assert_eq!(me["spend"]["spent_usd_micros"], 0, "{me}");

    let chatted = server.chat(&sam).await;
    assert!(
        chatted.to_string().contains("person who asked"),
        "the refusal says whose cap stopped it: {chatted}"
    );
    // The owner is held by the provider's cap alone.
    let owners = server.chat(OWNER_KEY).await;
    assert_eq!(
        owners["structuredContent"]["receipt"]["model_calls"][0]["model"], "capable",
        "{owners}"
    );
    assert!(server.me(OWNER_KEY).await.get("spend").is_none());
}

#[tokio::test]
async fn a_persons_cap_lets_them_spend_and_their_spend_shows() {
    let server = serve(0).await;
    assert_eq!(
        server.person("ana", Some(50_000)).await,
        StatusCode::CREATED
    );
    let ana = server.key("person:ana").await;
    let chatted = server.chat(&ana).await;
    assert_eq!(
        chatted["structuredContent"]["receipt"]["model_calls"][0]["model"], "capable",
        "{chatted}"
    );
    let me = server.me(&ana).await;
    assert_eq!(me["spend"]["day_cap_usd_micros"], 50_000, "{me}");
    let spent = me["spend"]["spent_usd_micros"].as_u64().unwrap();
    assert!(spent > 0, "{me}");
    let (_, people) = server
        .call(Method::GET, "/api/v1/people", OWNER_KEY, None)
        .await;
    assert_eq!(people["items"][0]["day_cap_usd_micros"], 50_000, "{people}");
}

#[tokio::test]
async fn the_default_cap_holds_for_everyone_without_their_own() {
    let server = serve(30_000).await;
    server.person("kim", None).await;
    server.person("lee", Some(0)).await;
    let kim = server.key("person:kim").await;
    let lee = server.key("person:lee").await;
    assert_eq!(server.me(&kim).await["spend"]["day_cap_usd_micros"], 30_000);
    let chatted = server.chat(&kim).await;
    assert_eq!(
        chatted["structuredContent"]["receipt"]["model_calls"][0]["model"], "capable",
        "{chatted}"
    );
    // A cap of their own, even zero, is theirs.
    assert_eq!(server.me(&lee).await["spend"]["day_cap_usd_micros"], 0);
    assert!(
        server
            .chat(&lee)
            .await
            .to_string()
            .contains("person who asked")
    );
}
