//! Asking for a capable model: a free model that can only chat, and a
//! priced one marked `investigative`. Routing takes the cheapest, so work
//! reaches the capable one only when it asks: an agent through its `needs`,
//! a plugin through its grant's `model_needs`.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use async_trait::async_trait;
use chaos_model::{ChaosServer, ChaosStep};
use events::MemorySink;
use model::Catalog;
use plugin_api::{
    Capabilities, ModelCapability, Plugin, PluginError, Tool, ToolContext, ToolDefinition,
    ToolFailure, ToolOutput,
};
use protocol::Trust;
use runtime::{Config, Plugins, PluginsConfig, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const MANIFEST: &str = r"api_version: 1
name: asker
version: 0.0.1
description: Asks a model, and says which answered.
capabilities: [model]
tools:
  - name: asker_ask
    description: Asks a model.
    effect: read
    input_schema: {type: object, additionalProperties: false, properties: {}}
    output_schema: {type: object, additionalProperties: false, required: [model], properties: {model: {type: string, maxLength: 64}}}
";

#[derive(Debug)]
struct Asker;

#[derive(Debug)]
struct Ask(Arc<dyn ModelCapability>);

#[async_trait]
impl Tool for Ask {
    fn definition(&self) -> ToolDefinition {
        plugin_api::PluginManifest::from_yaml(MANIFEST)
            .unwrap()
            .definitions()
            .remove(0)
    }

    async fn run(&self, _: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let reply = self
            .0
            .complete("Which are you?", context)
            .await
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        Ok(ToolOutput::new(
            json!({ "model": reply.model }),
            Trust::Untrusted,
        ))
    }
}

impl Plugin for Asker {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        Ok(vec![Arc::new(Ask(capabilities.model()?))])
    }
}

/// A runtime with both models, the plugin granted as `grant` says, and a
/// front door agent with `door_needs`.
async fn runtime(grant: &str, door_needs: &str) -> (Runtime, ChaosServer) {
    let server = ChaosServer::start(vec![ChaosStep::Text("I am a model.".to_owned()); 4])
        .await
        .unwrap();
    let url = &server.base_url;
    let yaml = format!(
        "version: 1\nproviders:\n\
         \x20 - {{name: free, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         \x20 - {{name: paid, base_url: '{url}', no_log: true, timeout_secs: 10, \
         spend_cap_usd_micros_per_day: 1000000, paid_for: [chat, 'plugin:asker']}}\n\
         models:\n\
         \x20 - {{name: cheap, provider: free, wire_model: 'm1', capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n\
         \x20 - {{name: capable, provider: paid, wire_model: 'm2', capabilities: [chat, tools, investigative], context_tokens: 100000, \
         max_output_tokens: 100, price: {{input_usd_micros_per_mtok: 1000, output_usd_micros_per_mtok: 1000}}}}\n"
    );
    let mut config = Config {
        recall_limit: 0,
        front_door: Some("door".to_owned()),
        ..Config::default()
    };
    config.budget.max_usd_micros = 100_000;
    let store = Arc::new(MemoryStore::new(3));
    let runtime = Runtime::build(
        Arc::clone(&store) as Arc<dyn store::Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
        Plugins {
            compiled: vec![Arc::new(Asker)],
            grants: PluginsConfig::from_yaml(&format!("version: 1\nplugins:\n  - {grant}\n"))
                .unwrap(),
        },
    )
    .await
    .unwrap();
    let root = std::env::temp_dir().join(format!("model-needs-{}", uuid::Uuid::new_v4()));
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let door = format!("description: Talks.\nsystem_prompt: You talk.\ntools: []\n{door_needs}");
    std::fs::write(root.join("agents/door.yaml"), door).unwrap();
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    (runtime, server)
}

async fn call(runtime: &Runtime, tool: &str, arguments: &Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
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
    serde_json::from_slice::<Value>(&output).unwrap()["result"].take()
}

async fn plugin_model(runtime: &Runtime) -> Value {
    let asked = call(runtime, "asker_ask", &json!({})).await;
    asked["structuredContent"]["result"]["model"].clone()
}

async fn door_model(runtime: &Runtime) -> Value {
    let chatted = call(runtime, "chat", &json!({ "message": "hello" })).await;
    chatted["structuredContent"]["receipt"]["model_calls"][0]["model"].clone()
}

#[tokio::test]
async fn work_that_asks_for_nothing_goes_to_the_cheapest() {
    let (runtime, _server) = runtime("{name: asker, capabilities: [model]}", "").await;
    assert_eq!(plugin_model(&runtime).await, "cheap");
    assert_eq!(door_model(&runtime).await, "cheap");
}

#[tokio::test]
async fn work_that_asks_for_a_capable_model_gets_one() {
    let (runtime, _server) = runtime(
        "{name: asker, capabilities: [model], model_needs: [investigative]}",
        "needs: [investigative]\n",
    )
    .await;
    assert_eq!(plugin_model(&runtime).await, "capable");
    assert_eq!(door_model(&runtime).await, "capable");
}
