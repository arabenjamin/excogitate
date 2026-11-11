//! The `http` and `model` capabilities, through the real host and runtime.
//! The test plugin claims its output is trusted and free; the host has to
//! label it untrusted and charge what its model calls cost.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::routing::get;
use chaos_model::{ChaosServer, ChaosStep};
use events::MemorySink;
use model::Catalog;
use plugin_api::{
    Capabilities, HttpCapability, HttpRequest, ModelCapability, Plugin, PluginError, Tool,
    ToolContext, ToolDefinition, ToolFailure, ToolOutput,
};
use protocol::Trust;
use runtime::{Config, Plugins, PluginsConfig, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;
use tools::HttpConfig;

const MANIFEST: &str = r"api_version: 1
name: probe
version: 0.0.1
description: Tests capabilities.
capabilities: [http, model]
tools:
  - name: probe_fetch
    description: Fetches a URL.
    effect: read
    input_schema: {type: object, additionalProperties: false, required: [url], properties: {url: {type: string, maxLength: 2048}}}
    output_schema: {type: object, additionalProperties: false, required: [status, body], properties: {status: {type: integer}, body: {type: string, maxLength: 100000}}}
  - name: probe_ask
    description: Asks a model.
    effect: read
    input_schema: {type: object, additionalProperties: false, required: [prompt], properties: {prompt: {type: string, maxLength: 1000}}}
    output_schema: {type: object, additionalProperties: false, required: [text], properties: {text: {type: string, maxLength: 100000}}}
";

#[derive(Debug)]
struct Probe;

#[derive(Debug)]
struct Fetch(Arc<dyn HttpCapability>);

#[derive(Debug)]
struct Ask(Arc<dyn ModelCapability>);

fn definition(name: &str) -> ToolDefinition {
    plugin_api::PluginManifest::from_yaml(MANIFEST)
        .unwrap()
        .definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .unwrap()
}

/// What the plugin claims: trusted, and free. The host must not believe it.
fn claimed(value: Value) -> ToolOutput {
    ToolOutput {
        value,
        trust: Trust::Trusted,
        cost_usd_micros: 0,
    }
}

#[async_trait]
impl Tool for Fetch {
    fn definition(&self) -> ToolDefinition {
        definition("probe_fetch")
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let request = HttpRequest {
            method: "get".to_owned(),
            url: arguments["url"].as_str().unwrap().to_owned(),
            headers: Vec::new(),
            body: None,
        };
        let response = self
            .0
            .request(request, context)
            .await
            .map_err(|error| ToolFailure(error.to_string()))?;
        Ok(claimed(
            json!({ "status": response.status, "body": response.body }),
        ))
    }
}

#[async_trait]
impl Tool for Ask {
    fn definition(&self) -> ToolDefinition {
        definition("probe_ask")
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let reply = self
            .0
            .complete(arguments["prompt"].as_str().unwrap(), context)
            .await
            .map_err(|error| ToolFailure(error.to_string()))?;
        Ok(claimed(json!({ "text": reply.text })))
    }
}

impl Plugin for Probe {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        Ok(vec![
            Arc::new(Fetch(capabilities.http()?)),
            Arc::new(Ask(capabilities.model()?)),
        ])
    }
}

const PAGE: &str = "Sea otters hold hands.";

async fn page_url() -> String {
    let router = Router::new().route("/otters", get(|| async { PAGE }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}/otters")
}

/// A priced model, which plugin `probe` may pay for when `paid` says so.
async fn runtime(script: Vec<ChaosStep>, paid: &str) -> (Runtime, ChaosServer, Arc<MemoryStore>) {
    let server = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10, \
         spend_cap_usd_micros_per_day: 1000000, paid_for: [{paid}]}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000000, \
         max_output_tokens: 100, price: {{input_usd_micros_per_mtok: 1000000, output_usd_micros_per_mtok: 1000000}}}}\n",
        server.base_url
    );
    let mut config = Config {
        http: HttpConfig {
            allowed_ranges: vec!["127.0.0.1/32".to_owned()],
            ..HttpConfig::default()
        },
        recall_limit: 0,
        ..Config::default()
    };
    config.budget.max_usd_micros = 1_000_000;
    let store = Arc::new(MemoryStore::new(3));
    let runtime = Runtime::build(
        Arc::clone(&store) as Arc<dyn store::Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        config,
        &|_| None,
        Plugins {
            compiled: vec![Arc::new(Probe)],
            grants: PluginsConfig::from_yaml(
                "version: 1\nplugins:\n  - {name: probe, capabilities: [http, model]}\n",
            )
            .unwrap(),
        },
    )
    .await
    .unwrap();
    (runtime, server, store)
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
async fn http_goes_through_the_network_rules_and_comes_back_untrusted() {
    let (runtime, _server, _) = runtime(vec![], "chat").await;
    let fetched = call(&runtime, "probe_fetch", &json!({ "url": page_url().await })).await;
    assert_eq!(fetched["isError"], false, "{fetched}");
    let envelope = &fetched["structuredContent"];
    assert_eq!(envelope["result"]["body"], PAGE, "{fetched}");
    assert_eq!(
        envelope["untrusted"], true,
        "the plugin's claim of trust is overridden"
    );

    // An address the rules don't allow is refused for a plugin as for a model.
    let refused = call(
        &runtime,
        "probe_fetch",
        &json!({ "url": "http://10.0.0.1/" }),
    )
    .await;
    assert_eq!(refused["isError"], true, "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugins_model_call_is_charged_to_the_run_whatever_it_reports() {
    let script = vec![
        ChaosStep::ToolCalls(vec![(
            "probe_ask".to_owned(),
            json!({ "prompt": "say hi" }).to_string(),
        )]),
        ChaosStep::Text("hi from the plugin's model".to_owned()),
        ChaosStep::Text("done".to_owned()),
    ];
    let (runtime, _server, store) = runtime(script, "chat, 'plugin:probe'").await;
    let chatted = call(&runtime, "chat", &json!({ "message": "use the probe" })).await;
    let receipt = &chatted["structuredContent"]["receipt"];
    let tool_call = &receipt["tool_calls"][0];
    assert_eq!(tool_call["tool"], "probe_ask", "{chatted}");
    assert_eq!(tool_call["status"], "ran", "{chatted}");
    assert_eq!(tool_call["untrusted"], true, "{chatted}");
    let charged = tool_call["cost_usd_micros"].as_u64().unwrap();
    assert!(
        charged > 0,
        "the plugin said 0, and the host charged what it cost: {chatted}"
    );
    let models: u64 = receipt["model_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["cost_usd_micros"].as_u64().unwrap())
        .sum();
    assert_eq!(
        receipt["totals"]["cost_usd_micros"].as_u64().unwrap(),
        models + charged
    );
    let spent = store::SpendLog::spent_on(store.as_ref(), "c", chrono::Utc::now().date_naive())
        .await
        .unwrap();
    assert_eq!(
        spent,
        models + charged,
        "the day's ledger saw the same calls"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_pays_only_if_the_provider_lets_it() {
    let (runtime, _server, _) = runtime(vec![], "chat").await;
    let refused = call(&runtime, "probe_ask", &json!({ "prompt": "say hi" })).await;
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("may not pay"), "{refused}");
}
