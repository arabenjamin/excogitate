//! The `memory` capability through the real host and runtime: a plugin's
//! note is found by the runtime's own memory search, and a plugin not
//! granted the capability doesn't start.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use async_trait::async_trait;
use events::MemorySink;
use model::Catalog;
use plugin_api::{
    Capabilities, MemoryCapability, NoteToStore, Plugin, PluginError, Tool, ToolContext,
    ToolDefinition, ToolFailure, ToolOutput,
};
use protocol::Trust;
use runtime::{Config, Plugins, PluginsConfig, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const MANIFEST: &str = r"api_version: 1
name: keeper
version: 0.0.1
description: Keeps notes.
capabilities: [memory]
tools:
  - name: keeper_keep
    description: Keeps a note.
    effect: write
    input_schema: {type: object, additionalProperties: false, required: [text], properties: {text: {type: string, maxLength: 1000}}}
    output_schema: {type: object, additionalProperties: false, required: [id, created], properties: {id: {type: string, maxLength: 64}, created: {type: boolean}}}
";

const MODELS: &str = "version: 1\nproviders:\n  - {name: c, base_url: 'http://127.0.0.1:9/v1', no_log: true, timeout_secs: 1}\n\
                      models:\n  - {name: c, provider: c, wire_model: x, capabilities: [chat], context_tokens: 1000, max_output_tokens: 10}\n";

#[derive(Debug)]
struct Keeper;

#[derive(Debug)]
struct Keep(Arc<dyn MemoryCapability>);

#[async_trait]
impl Tool for Keep {
    fn definition(&self) -> ToolDefinition {
        plugin_api::PluginManifest::from_yaml(MANIFEST)
            .unwrap()
            .definitions()
            .remove(0)
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let text = arguments["text"].as_str().unwrap();
        let stored = self
            .0
            .store(NoteToStore::new("", text), context)
            .await
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        Ok(ToolOutput::new(
            json!({ "id": stored.id.to_string(), "created": stored.created }),
            Trust::Trusted,
        ))
    }
}

impl Plugin for Keeper {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        Ok(vec![Arc::new(Keep(capabilities.memory()?))])
    }
}

async fn runtime(grants: &str) -> Result<Runtime, runtime::RuntimeError> {
    Runtime::build(
        Arc::new(MemoryStore::new(3)) as Arc<dyn store::Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(MODELS, |_| None).unwrap(),
        Config {
            recall_limit: 0,
            ..Config::default()
        },
        &|_| None,
        Plugins {
            compiled: vec![Arc::new(Keeper)],
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

#[tokio::test(flavor = "multi_thread")]
async fn a_plugins_note_is_found_by_memory_search() {
    let runtime = runtime("version: 1\nplugins:\n  - {name: keeper, capabilities: [memory]}\n")
        .await
        .unwrap();
    let kept = call(
        &runtime,
        "keeper_keep",
        &json!({ "text": "Sea otters hold hands." }),
    )
    .await;
    assert_eq!(
        kept["structuredContent"]["result"]["created"], true,
        "{kept}"
    );
    let found = call(
        &runtime,
        "memory",
        &json!({ "operation": "search", "query": "otters" }),
    )
    .await;
    let hits = &found["structuredContent"]["result"]["hits"];
    assert_eq!(hits.as_array().unwrap().len(), 1, "{found}");
    assert_eq!(
        hits[0]["trust"], "untrusted",
        "a plugin's note is never trusted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_not_granted_memory_does_not_start() {
    let refused = runtime("version: 1\nplugins:\n  - {name: keeper, capabilities: []}\n").await;
    let error = refused
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(error.contains("Memory"), "the refusal: {error:?}");
}
