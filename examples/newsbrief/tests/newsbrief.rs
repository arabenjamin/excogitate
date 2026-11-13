//! newsbrief's tools through the real host and dispatcher, on the memory
//! store. The workflow they serve is tested end to end in tests/brief.rs.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use dispatch::{Budget, CallStatus, Session, SessionConfig, ToolRegistry};
use events::MemorySink;
use model::ToolCall;
use plugin_api::{DataClass, PageRequest, RecordQuery, RunId};
use plugin_host::{HostError, PluginHost, PluginsConfig};
use plugin_newsbrief::NewsbriefPlugin;
use serde_json::{Value, json};
use store::{RecordReader, Store};
use store_memory::MemoryStore;

const GRANTED: &str = "version: 1\nplugins:\n  - {name: newsbrief, capabilities: [records]}\n";

const BUDGET: Budget = Budget {
    max_rounds: 3,
    max_tool_calls: 20,
    max_writes: 10,
    max_total_tokens: 10_000,
    max_usd_micros: 0,
    max_request_bytes: 100_000,
    max_duration_ms: 20_000,
};

fn host(store: &Arc<MemoryStore>, config: &str) -> PluginHost {
    PluginHost::new(
        Arc::clone(store) as Arc<dyn Store>,
        PluginsConfig::from_yaml(config).unwrap(),
        ToolRegistry::new(),
    )
}

fn session(registry: ToolRegistry, offered: &[&str]) -> Session {
    Session::new(
        Arc::new(registry),
        Arc::new(MemorySink::new()),
        SessionConfig {
            run: RunId::generate(),
            offered: offered.iter().map(|name| (*name).to_owned()).collect(),
            budget: BUDGET,
            data_class: DataClass::Private,
            purpose: "test".to_owned(),
            max_result_chars: 20_000,
            subject: dispatch::RunSubject {
                agent: "test".to_owned(),
                harness: "test".to_owned(),
            },
            probe: None,
        },
    )
    .unwrap()
}

async fn call(session: &mut Session, name: &str, arguments: &Value) -> (CallStatus, String) {
    let call = ToolCall {
        id: "c".to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_string(),
    };
    let done = session.dispatch(&call).await;
    (done.status, done.content)
}

#[tokio::test]
async fn sections_are_kept_in_order_replaced_by_name_and_removed() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, GRANTED);
    host.install_compiled(&NewsbriefPlugin).await.unwrap();
    let tools = [
        "newsbrief_sections",
        "newsbrief_section_set",
        "newsbrief_section_remove",
    ];
    let mut session = session(host.into_registry(), &tools);
    let body = |content: &str| serde_json::from_str::<Value>(content).unwrap()["result"].take();

    // None yet: the failure says how to add one.
    let (status, content) = call(&mut session, "newsbrief_sections", &json!({})).await;
    assert_ne!(status, CallStatus::Ran);
    assert!(content.contains("newsbrief_section_set"), "{content}");

    for args in [
        json!({"name": "Science", "query": "science research", "order": 2}),
        json!({"name": "World", "query": "world news", "sites": ["apnews.com"], "order": 1}),
        json!({"name": "Science", "query": "space and science", "order": 2}),
    ] {
        let (status, content) = call(&mut session, "newsbrief_section_set", &args).await;
        assert_eq!(status, CallStatus::Ran, "{content}");
    }
    let (_, content) = call(&mut session, "newsbrief_sections", &json!({})).await;
    let listed = body(&content);
    assert_eq!(listed["date"].as_str().unwrap().len(), 10, "{listed}");
    assert_eq!(
        listed["sections"],
        json!([
            {"name": "World", "query": "world news", "sites": ["apnews.com"], "order": 1},
            {"name": "Science", "query": "space and science", "sites": [], "order": 2}
        ])
    );
    let found = store
        .query_records(RecordQuery::new(
            "newsbrief.section",
            PageRequest::new(10, 0).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(found.items.len(), 2, "a replaced section isn't kept twice");

    let (_, content) = call(
        &mut session,
        "newsbrief_section_remove",
        &json!({"name": "World"}),
    )
    .await;
    assert_eq!(body(&content), json!({"removed": true}));
    let (_, content) = call(
        &mut session,
        "newsbrief_section_remove",
        &json!({"name": "World"}),
    )
    .await;
    assert_eq!(body(&content), json!({"removed": false}));
}

#[tokio::test]
async fn without_the_grant_it_does_not_install() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, "version: 1\nplugins: []\n");
    let error = host.install_compiled(&NewsbriefPlugin).await.unwrap_err();
    assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
}
