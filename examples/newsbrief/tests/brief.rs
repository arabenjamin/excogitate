//! The Daily News Brief end to end: the real runtime loads the definitions
//! in `definitions/`, the workflow is started over REST and run by a worker,
//! and the brief comes out and is kept in memory. The model is scripted, and
//! `search_web` answers from a script through the search example's own
//! manifest, so the contract the workflow is written against is the real one.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaos_model::{ChaosServer, ChaosStep};
use chrono::Utc;
use events::MemorySink;
use model::Catalog;
use plugin_api::{
    Capabilities, Plugin, PluginError, PluginManifest, Tool, ToolContext, ToolDefinition,
    ToolFailure, ToolOutput, Trust,
};
use plugin_newsbrief::NewsbriefPlugin;
use reqwest::Client;
use runtime::{Config, Plugins, PluginsConfig, Runtime};
use serde_json::{Value, json};
use store::Store;
use store_memory::MemoryStore;
use tokio::sync::watch;
use work::{StoreTransport, Worker, WorkerOptions};

const KEY: &str = "an-owner-key-of-sufficient-length-1234";
const BRIEF: &str =
    "## World\n- A treaty was signed (https://apnews.com/t)\n\n## Science\nNothing new found.";

/// `search_web` as the search example declares it, answering from a script:
/// World's search finds one story, Science's fails. Each query is kept.
#[derive(Debug, Default)]
struct ScriptedSearch {
    asked: Arc<Mutex<Vec<Value>>>,
}

#[derive(Debug)]
struct SearchTool {
    definition: ToolDefinition,
    asked: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        self.asked.lock().unwrap().push(arguments.clone());
        if arguments["query"] == "science" {
            return Err(ToolFailure::new("HTTP 429, rate limited"));
        }
        Ok(ToolOutput::new(
            json!({"engine": "scripted", "tried": [], "results": [
                {"title": "A treaty was signed", "url": "https://apnews.com/t", "snippet": "Signed today."}
            ]}),
            Trust::Untrusted,
        ))
    }
}

impl Plugin for ScriptedSearch {
    fn manifest(&self) -> &str {
        plugin_search::MANIFEST
    }

    fn tools(&self, _: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        let manifest = PluginManifest::from_yaml(plugin_search::MANIFEST)
            .map_err(|error| PluginError::Other(error.to_string()))?;
        Ok(manifest
            .definitions()
            .into_iter()
            .map(|definition| {
                Arc::new(SearchTool {
                    definition,
                    asked: Arc::clone(&self.asked),
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

const GRANTS: &str = "version: 1\nplugins:\n  - {name: newsbrief, capabilities: [records]}\n  - {name: search, capabilities: [http]}\n";

struct Running {
    url: String,
    client: Client,
    runtime: Runtime,
    store: Arc<MemoryStore>,
    _model: ChaosServer,
}

impl Running {
    async fn post(&self, path: &str, body: &Value) -> Value {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth(KEY)
            .json(body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn mcp(&self, tool: &str, arguments: &Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": tool, "arguments": arguments}});
        let mut output = Vec::new();
        plugin_api::serve(
            "test",
            "1",
            self.runtime.mcp_tools(),
            format!("{request}\n").as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        serde_json::from_slice::<Value>(&output).unwrap()["result"].take()
    }

    /// Runs a worker until `job` has ended, and returns it.
    async fn work_off(&self, job: &str) -> Value {
        let worker = Arc::new(
            Worker::new(
                Arc::new(StoreTransport(Arc::clone(&self.store) as Arc<dyn Store>)),
                self.runtime.job_handlers(),
                WorkerOptions {
                    poll: Duration::from_millis(5),
                    ..WorkerOptions::default()
                },
                "test".to_owned(),
            )
            .unwrap(),
        );
        let (stop, shutdown) = watch::channel(false);
        let running = tokio::spawn(worker.run(shutdown));
        let mut seen = Value::Null;
        for _ in 0..500 {
            seen = self.get(&format!("/api/v1/jobs/{job}")).await;
            if seen["status"] == "succeeded" || seen["status"] == "dead" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.send(true).unwrap();
        running.await.unwrap();
        seen
    }
}

async fn start(search: ScriptedSearch, script: Vec<ChaosStep>) -> Running {
    let model = ChaosServer::start(script).await.unwrap();
    let models = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools, investigative], context_tokens: 1000000, max_output_tokens: 4000}}\n",
        model.base_url
    );
    let store = Arc::new(MemoryStore::new(3));
    let runtime = Runtime::build(
        Arc::clone(&store) as Arc<dyn Store>,
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&models, |_| None).unwrap(),
        Config {
            recall_limit: 0,
            ..Config::default()
        },
        &|_| None,
        Plugins {
            compiled: vec![Arc::new(NewsbriefPlugin), Arc::new(search)],
            grants: PluginsConfig::from_yaml(GRANTS).unwrap(),
        },
    )
    .await
    .unwrap();
    // The definitions as they ship.
    let definitions = Path::new(env!("CARGO_MANIFEST_DIR")).join("definitions");
    runtime
        .load_definitions(&definitions, Utc::now())
        .await
        .unwrap();
    let router = runtime.http_router("test", "1", KEY).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Running {
        url,
        client: Client::new(),
        runtime,
        store,
        _model: model,
    }
}

#[tokio::test]
async fn the_brief_searches_each_section_writes_it_and_keeps_it() {
    let search = ScriptedSearch::default();
    let asked = Arc::clone(&search.asked);
    let running = start(search, vec![ChaosStep::Text(BRIEF.to_owned())]).await;
    for section in [
        json!({"name": "World", "query": "world news", "sites": ["apnews.com"], "order": 1}),
        json!({"name": "Science", "query": "science", "order": 2}),
    ] {
        let set = running.mcp("newsbrief_section_set", &section).await;
        assert_eq!(set["isError"], false, "{set}");
    }

    let started = running
        .post("/api/v1/workflows/newsbrief/runs", &json!({"inputs": {}}))
        .await;
    let done = running.work_off(started["job_id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let run = &done["result"];
    assert_eq!(run["output"], BRIEF);
    let statuses: Vec<&Value> = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| &step["status"])
        .collect();
    assert!(statuses.iter().all(|status| *status == "ran"), "{run}");

    // One search per section, in order, each as the workflow writes it.
    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [
            json!({"query": "world news", "sites": ["apnews.com"], "news": true, "recency": "day", "limit": 6}),
            json!({"query": "science", "sites": [], "news": true, "recency": "day", "limit": 6}),
        ]
    );
    // Science's search failed and left its entry null; the brief went on.
    let search = &run["steps"][1];
    assert_eq!(search["items"][0]["status"], "ran", "{search}");
    assert_eq!(search["items"][1]["status"], "failed");

    // The brief is in memory, tagged, for later runs to find.
    let found = running
        .mcp(
            "memory",
            &json!({"operation": "search", "query": "treaty signed"}),
        )
        .await;
    let text = found.to_string();
    assert!(
        text.contains("A treaty was signed") && text.contains("newsbrief"),
        "{text}"
    );
}

#[tokio::test]
async fn a_brief_with_no_sections_fails_saying_how_to_add_one() {
    let running = start(ScriptedSearch::default(), vec![]).await;
    let started = running
        .post("/api/v1/workflows/newsbrief/runs", &json!({"inputs": {}}))
        .await;
    let done = running.work_off(started["job_id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "dead", "{done}");
    let error = done["last_error"].as_str().unwrap();
    assert!(
        error.starts_with("step sections:") && error.contains("newsbrief_section_set"),
        "{error}"
    );
}
