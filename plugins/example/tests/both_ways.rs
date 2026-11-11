//! The example plugin, run both ways through the real host and dispatcher.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use dispatch::{Budget, CallStatus, Outcome, Session, SessionConfig, ToolRegistry};
use events::MemorySink;
use model::ToolCall;
use plugin_example::{ExamplePlugin, SERVICE_MANIFEST};
use plugin_host::{HostError, PluginHost, PluginsConfig, ServiceSpec};
use protocol::{DataClass, RunId};
use serde_json::{Value, json};
use store::{FieldFilter, FieldOp, FieldValue, PageRequest, RecordQuery, RecordReader, Store};
use store_memory::MemoryStore;

const BUDGET: Budget = Budget {
    max_rounds: 3,
    max_tool_calls: 8,
    max_writes: 2,
    max_total_tokens: 10_000,
    max_usd_micros: 0,
    max_request_bytes: 10_000,
    max_duration_ms: 20_000,
};

fn config(text: &str) -> PluginsConfig {
    PluginsConfig::from_yaml(text).unwrap()
}

const GRANTED: &str = "version: 1\nplugins:\n  - {name: example, capabilities: [records]}\n";

fn session(registry: ToolRegistry, offered: &[&str]) -> Session {
    Session::new(
        Arc::new(registry),
        Arc::new(MemorySink::new()),
        SessionConfig {
            run: RunId::generate(),
            offered: offered.iter().map(|s| (*s).to_owned()).collect(),
            budget: BUDGET,
            data_class: DataClass::Private,
            purpose: "test".to_owned(),
            max_result_chars: 2000,
            subject: dispatch::RunSubject {
                agent: "test".to_owned(),
                harness: "test".to_owned(),
            },
            probe: None,
        },
    )
    .unwrap()
}

fn call(name: &str, arguments: &Value) -> ToolCall {
    ToolCall {
        id: "c".to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_string(),
    }
}

fn body(content: &str) -> Value {
    serde_json::from_str(content).unwrap()
}

#[tokio::test]
async fn compiled_in_it_counts_and_keeps_bookmarks() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = PluginHost::new(
        Arc::clone(&store) as Arc<dyn Store>,
        config(GRANTED),
        ToolRegistry::new(),
    );
    host.install_compiled(&ExamplePlugin).await.unwrap();
    let mut session = session(
        host.into_registry(),
        &["example_word_count", "example_bookmark_add"],
    );

    let count = session
        .dispatch(&call(
            "example_word_count",
            &json!({ "text": "one two three" }),
        ))
        .await;
    assert_eq!(body(&count.content)["result"]["words"], 3);

    let url = "https://example.com/a";
    let added = session
        .dispatch(&call("example_bookmark_add", &json!({ "url": url })))
        .await;
    assert_eq!(added.status, CallStatus::Ran, "{}", added.content);

    let found = store
        .query_records(RecordQuery {
            record_type: "example.bookmark".to_owned(),
            filters: vec![FieldFilter {
                field: "url".to_owned(),
                op: FieldOp::Eq,
                value: FieldValue::Text(url.to_owned()),
            }],
            page: PageRequest::new(5, 0).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(
        found.items.first().unwrap().provenance.stored_by.as_str(),
        "plugin:example"
    );

    // The model can't reach past the schema, and can't choose the privacy class.
    let sneaky = session
        .dispatch(&call(
            "example_bookmark_add",
            &json!({ "url": url, "data_class": "public" }),
        ))
        .await;
    assert_eq!(sneaky.status, CallStatus::Refused);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.totals.writes, 1);
}

#[tokio::test]
async fn without_the_grant_it_does_not_install() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = PluginHost::new(
        store as Arc<dyn Store>,
        config("version: 1\nplugins: []\n"),
        ToolRegistry::new(),
    );
    let error = host.install_compiled(&ExamplePlugin).await.unwrap_err();
    assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
}

fn service_spec() -> ServiceSpec {
    ServiceSpec {
        command: env!("CARGO_BIN_EXE_example-service").to_owned(),
        args: vec![],
        env_allow: vec![],
        timeout_secs: 10,
    }
}

fn service_host() -> PluginHost {
    let store = Arc::new(MemoryStore::new(3));
    PluginHost::new(
        store as Arc<dyn Store>,
        config("version: 1\nplugins: []\n"),
        ToolRegistry::new(),
    )
}

#[tokio::test]
async fn as_a_service_it_runs_in_its_own_process() {
    let mut host = service_host();
    host.install_service(SERVICE_MANIFEST, &service_spec())
        .await
        .unwrap();
    let mut session = session(host.into_registry(), &["example_word_count"]);
    let reply = session
        .dispatch(&call("example_word_count", &json!({ "text": "a b" })))
        .await;
    assert_eq!(reply.status, CallStatus::Ran, "{}", reply.content);
    let parsed = body(&reply.content);
    assert_eq!(parsed["result"]["words"], 2);
    assert_eq!(parsed["untrusted"], true);
}

#[tokio::test]
async fn a_service_serving_a_different_schema_is_refused() {
    let mut host = service_host();
    let tampered = SERVICE_MANIFEST.replace("maxLength: 100000", "maxLength: 5");
    let error = host
        .install_service(&tampered, &service_spec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("differs"), "{error}");
}

#[tokio::test]
async fn a_service_serving_undeclared_tools_is_refused() {
    let mut host = service_host();
    let (head, _) = SERVICE_MANIFEST.split_once("tools:").unwrap();
    let none = format!("{head}tools: []\n");
    let error = host
        .install_service(&none, &service_spec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("doesn't declare"), "{error}");
}

#[tokio::test]
async fn a_service_that_cannot_start_says_so() {
    let mut host = service_host();
    let mut spec = service_spec();
    spec.command = "/nonexistent/plugin".to_owned();
    let error = host
        .install_service(SERVICE_MANIFEST, &spec)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("couldn't start"), "{error}");
}

#[tokio::test]
async fn a_service_that_stops_answering_fails_visibly() {
    let mut host = service_host();
    let mut spec = service_spec();
    // A program that exits at once, in place of a server.
    spec.command = "/bin/true".to_owned();
    let error = host
        .install_service(SERVICE_MANIFEST, &spec)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
}
