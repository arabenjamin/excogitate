//! A remote MCP server's tools in the built runtime: listed in the config,
//! registered at startup, named by an agent's definition, and called like
//! any tool.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use axum::Json;
use events::MemorySink;
use model::Catalog;
use runtime::{Config, Plugins, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

/// A server with one tool, `search`, that echoes its query.
async fn server() -> String {
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(|Json(message): Json<Value>| async move {
            let Some(id) = message.get("id").cloned() else {
                return (axum::http::StatusCode::ACCEPTED, Json(json!({})));
            };
            let result = match message["method"].as_str() {
                Some("initialize") => json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
                Some("tools/list") => {
                    json!({"tools": [{"name": "search", "description": "Searches.",
                    "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}}]})
                }
                _ => json!({"content": [{"type": "text",
                    "text": format!("found {}", message["params"]["arguments"]["q"])}]}),
            };
            (
                axum::http::StatusCode::OK,
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn config(url: &str) -> Config {
    Config::from_yaml(&format!(
        "recall_limit: 0\n\
         http:\n  allowed_ranges: ['127.0.0.0/8']\n\
         mcp_servers:\n\
         \x20 - {{name: docs, url: '{url}', allow_insecure: true, no_log: true, tools: [search], read_only: [search]}}\n"
    ))
    .unwrap()
}

async fn built(config: Config) -> Result<Runtime, String> {
    Runtime::build(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml("version: 1\nproviders: []\nmodels: []\n", |_| None).unwrap(),
        config,
        &|_| None,
        Plugins::default(),
    )
    .await
    .map_err(|error| error.to_string())
}

#[tokio::test]
async fn a_remote_tool_is_one_of_the_runtimes_own() {
    let runtime = built(config(&server().await)).await.unwrap();

    // A definition may name it.
    let root = std::env::temp_dir().join(format!("mcp-servers-{}", uuid::Uuid::new_v4()));
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(
        root.join("agents/reader.yaml"),
        "description: Reads.\nsystem_prompt: You read.\ntools: [docs_search]\n",
    )
    .unwrap();
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);

    // And it's called like any tool.
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": "docs_search", "arguments": {"q": "rust"}}});
    let mut output = Vec::new();
    plugin_api::serve(
        "test",
        "1",
        runtime.mcp_tools(),
        format!("{request}\n").as_bytes(),
        &mut output,
    )
    .await
    .unwrap();
    let reply: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"]["result"]["text"], "found \"rust\"",
        "{reply}"
    );
}

#[tokio::test]
async fn a_setting_that_is_wrong_stops_startup() {
    let url = server().await;
    let mut wrong = config(&url);
    wrong.mcp_servers[0].tools = vec!["delete".to_owned()];
    wrong.mcp_servers[0].read_only.clear();
    let error = built(wrong).await.unwrap_err();
    assert!(
        error.contains("MCP server docs: it doesn't offer delete; it offers search"),
        "{error}"
    );

    let unknown = Config::from_yaml(&format!(
        "mcp_servers:\n  - {{name: docs, url: '{url}', tools: [search], shell: true}}\n"
    ));
    assert!(unknown.is_err(), "an unknown key is refused");
}
