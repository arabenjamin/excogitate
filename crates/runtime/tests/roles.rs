//! An agent that asks for a role runs on the role's models, in the role's
//! order, not on the cheapest; one asking for a role models.yaml doesn't
//! define doesn't load.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
#![allow(missing_docs, reason = "a test")]

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

/// A model server that says "done" and keeps which wire model each call
/// asked for.
async fn server() -> (String, Arc<Mutex<Vec<String>>>) {
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(asked): State<Arc<Mutex<Vec<String>>>>, body: axum::Json<Value>| async move {
                    let wire = body.0["model"].as_str().unwrap_or_default().to_owned();
                    asked.lock().unwrap().push(wire);
                    axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": "done"},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2}
                    }))
                },
            ),
        )
        .with_state(Arc::clone(&asked));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/v1"), asked)
}

fn definitions(name: &str, door: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("roles-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(root.join("agents/door.yaml"), door).unwrap();
    root
}

fn runtime(url: &str) -> Runtime {
    // Both free, so without a role the first listed, `cheap`, wins.
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: p, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         models:\n\
         \x20 - {{name: cheap, provider: p, wire_model: wire-cheap, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n\
         \x20 - {{name: best, provider: p, wire_model: wire-best, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n\
         roles:\n  - {{name: writer, models: [best, cheap]}}\n"
    );
    Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Config {
            front_door: Some("door".to_owned()),
            recall_limit: 0,
            ..Config::default()
        },
        &|_| None,
    )
    .unwrap()
}

async fn chat(runtime: &Runtime) {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": "chat", "arguments": {"message": "hi"}}});
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
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_a_role_runs_on_the_roles_first_model() {
    for (name, door, wire) in [
        (
            "with",
            "description: d\nsystem_prompt: s\ntools: [memory]\nrole: writer\n",
            "wire-best",
        ),
        (
            "without",
            "description: d\nsystem_prompt: s\ntools: [memory]\n",
            "wire-cheap",
        ),
    ] {
        let (url, asked) = server().await;
        let runtime = runtime(&url);
        runtime
            .load_definitions(&definitions(name, door), chrono::Utc::now())
            .await
            .unwrap();
        chat(&runtime).await;
        assert_eq!(asked.lock().unwrap().as_slice(), [wire], "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_asking_for_a_role_thats_not_defined_doesnt_load() {
    let (url, _) = server().await;
    let error = runtime(&url)
        .load_definitions(
            &definitions(
                "undefined",
                "description: d\nsystem_prompt: s\ntools: [memory]\nrole: judge\n",
            ),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    let error = error.to_string();
    assert!(error.contains("its role judge isn't defined"), "{error}");
    assert!(
        error.contains("writer"),
        "names the roles there are: {error}"
    );
}
