//! Plugins in the running runtime: installed through the host, granted only
//! what `plugins.yaml` says, and reachable over MCP like the bundled tools.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use events::MemorySink;
use model::Catalog;
use plugin_example::ExamplePlugin;
use runtime::{Config, HarnessConfig, Plugins, PluginsConfig, Runtime, RuntimeError};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const MODELS: &str = "version: 1\nproviders:\n  - {name: c, base_url: 'http://127.0.0.1:9/v1', no_log: true, timeout_secs: 1}\n\
     models:\n  - {name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000, max_output_tokens: 10}\n";

const GRANTED: &str =
    "version: 1\nplugins:\n  - {name: example, capabilities: [records, sources]}\n";

async fn build(grants: &str, config: Config) -> Result<Runtime, RuntimeError> {
    Runtime::build(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(MODELS, |_| None).unwrap(),
        config,
        &|_| None,
        Plugins {
            compiled: vec![Arc::new(ExamplePlugin)],
            grants: PluginsConfig::from_yaml(grants).unwrap(),
        },
    )
    .await
}

async fn call(runtime: &Runtime, tool: &str, arguments: &Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let input = format!("{request}\n");
    let mut output = Vec::new();
    plugin_api::serve(
        "test",
        "1",
        runtime.mcp_tools(),
        input.as_bytes(),
        &mut output,
    )
    .await
    .unwrap();
    let line = String::from_utf8(output).unwrap();
    serde_json::from_str::<Value>(line.lines().next().unwrap()).unwrap()["result"].take()
}

#[tokio::test]
async fn a_granted_plugin_is_installed_and_its_tools_are_offered() {
    let runtime = build(GRANTED, Config::default()).await.unwrap();
    assert_eq!(runtime.plugins(), ["example"]);
    let names: Vec<String> = runtime
        .mcp_tools()
        .iter()
        .map(|tool| tool.definition().name)
        .collect();
    assert!(
        names.contains(&"example_word_count".to_owned()),
        "{names:?}"
    );
    assert!(
        names.contains(&"example_bookmark_add".to_owned()),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_plugin_tool_runs_through_dispatch_and_keeps_its_records() {
    let runtime = build(GRANTED, Config::default()).await.unwrap();
    let counted = call(
        &runtime,
        "example_word_count",
        &json!({ "text": "one two three" }),
    )
    .await;
    assert_eq!(counted["isError"], false, "{counted}");
    assert_eq!(
        counted["structuredContent"]["result"]["words"], 3,
        "{counted}"
    );
    let saved = call(
        &runtime,
        "example_bookmark_add",
        &json!({ "url": "https://example.com" }),
    )
    .await;
    assert_eq!(saved["isError"], false, "{saved}");
    // Its arguments are still checked against the manifest's schema.
    let refused = call(&runtime, "example_word_count", &json!({ "words": 1 })).await;
    assert_eq!(refused["isError"], true, "{refused}");
}

#[tokio::test]
async fn memory_finds_and_opens_what_a_plugins_source_holds() {
    let runtime = build(GRANTED, Config::default()).await.unwrap();
    let found = call(
        &runtime,
        "memory",
        &json!({ "operation": "search", "query": "what does a receipt hold" }),
    )
    .await;
    assert_eq!(found["isError"], false, "{found}");
    let envelope = &found["structuredContent"];
    assert_eq!(envelope["untrusted"], true, "a source's items never are");
    let first = &envelope["result"]["sources"][0];
    assert_eq!(first["source"], "example.glossary", "{envelope}");
    assert_eq!(first["id"], "receipt");
    assert_eq!(envelope["result"]["sources_skipped"], json!([]));

    let opened = call(
        &runtime,
        "memory",
        &json!({ "operation": "open", "source": "example.glossary", "item": "trajectory" }),
    )
    .await;
    assert_eq!(opened["isError"], false, "{opened}");
    let whole = &opened["structuredContent"]["result"];
    assert_eq!(whole["result"], "item", "{whole}");
    assert_eq!(whole["source"], "example.glossary");
    assert!(
        whole["text"].as_str().unwrap().starts_with("Every step"),
        "{whole}"
    );
    assert_eq!(whole["truncated"], false);

    let missing = call(
        &runtime,
        "memory",
        &json!({ "operation": "open", "source": "example.glossary", "item": "nothing" }),
    )
    .await;
    assert_eq!(missing["isError"], true, "{missing}");
}

#[tokio::test]
async fn a_plugin_without_its_grant_stops_the_build() {
    let error = build("version: 1\nplugins: []\n", Config::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("doesn't grant"), "{error}");
}

#[tokio::test]
async fn settings_can_name_a_plugin_tool() {
    let config = Config {
        harness: HarnessConfig {
            probe_reads: vec!["example_word_count".to_owned()],
            ..HarnessConfig::default()
        },
        ..Config::default()
    };
    assert!(build(GRANTED, config).await.is_ok());
}

fn manifest_file(name: &str, text: &str) -> String {
    let path = std::env::temp_dir().join(format!("plugin-{name}-{}.yaml", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

async fn build_processes(grants: &str) -> Result<Runtime, RuntimeError> {
    Runtime::build(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(MODELS, |_| None).unwrap(),
        Config::default(),
        &|_| None,
        Plugins {
            compiled: Vec::new(),
            grants: PluginsConfig::from_yaml(grants).unwrap(),
        },
    )
    .await
}

#[tokio::test]
async fn a_process_plugin_whose_manifest_names_another_is_refused() {
    let path = manifest_file("other", plugin_example::MANIFEST);
    let grants = format!(
        "version: 1\nplugins:\n  - name: notexample\n    capabilities: [records, sources]\n    process: {{command: /bin/true, manifest: '{path}'}}\n"
    );
    let error = build_processes(&grants).await.unwrap_err();
    assert!(error.to_string().contains("names it example"), "{error}");
}

#[tokio::test]
async fn a_process_plugin_without_its_manifest_says_which_file() {
    let grants = "version: 1\nplugins:\n  - name: example\n    capabilities: [records, sources]\n    process: {command: /bin/true, manifest: /nonexistent/plugin.yaml}\n";
    let error = build_processes(grants).await.unwrap_err();
    assert!(
        error.to_string().contains("/nonexistent/plugin.yaml"),
        "{error}"
    );
}

#[tokio::test]
async fn a_process_plugin_is_started_from_plugins_yaml() {
    // A program that exits at once: the host started it, and says it stopped.
    let path = manifest_file("start", plugin_example::MANIFEST);
    let grants = format!(
        "version: 1\nplugins:\n  - name: example\n    capabilities: [records, sources]\n    process: {{command: /bin/true, manifest: '{path}'}}\n"
    );
    let error = build_processes(&grants).await.unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
}
