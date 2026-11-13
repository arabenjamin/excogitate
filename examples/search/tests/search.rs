//! search through the real host and dispatcher, on the memory store, with
//! the search API answering from a script.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dispatch::{Budget, CallStatus, Session, SessionConfig, ToolRegistry};
use events::MemorySink;
use model::ToolCall;
use plugin_api::{
    CapabilityError, DataClass, HttpCapability, HttpRequest, HttpResponse, JobStarted, JobState,
    JobsCapability, MemoryCapability, ModelCapability, ModelReply, NoteStored, NoteToStore, RunId,
    RunWorkflow, ToolContext,
};
use plugin_host::{CapabilityMeter, CapabilitySource, HostError, PluginHost, PluginsConfig};
use plugin_search::SearchPlugin;
use serde_json::{Value, json};
use store::Store;
use store_memory::MemoryStore;

const GRANTED: &str = "version: 1\nplugins:\n  - {name: search, capabilities: [http]}\n";

const BUDGET: Budget = Budget {
    max_rounds: 3,
    max_tool_calls: 20,
    max_writes: 10,
    max_total_tokens: 10_000,
    max_usd_micros: 0,
    max_request_bytes: 100_000,
    max_duration_ms: 20_000,
};

/// The search API, answering every request with one status and body, and
/// keeping the addresses it was asked.
#[derive(Debug)]
struct TinyFish {
    status: u16,
    body: String,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl HttpCapability for TinyFish {
    async fn request(
        &self,
        request: HttpRequest,
        _: ToolContext,
    ) -> Result<HttpResponse, CapabilityError> {
        assert_eq!(request.method, "GET");
        self.asked.lock().unwrap().push(request.url);
        Ok(HttpResponse::new(self.status, self.body.clone()))
    }
}

/// Every other capability, refusing: none is granted.
#[derive(Debug)]
struct Refusing;

fn refused() -> CapabilityError {
    CapabilityError::Refused("not in this test".to_owned())
}

#[async_trait]
impl ModelCapability for Refusing {
    async fn complete(&self, _: &str, _: ToolContext) -> Result<ModelReply, CapabilityError> {
        Err(refused())
    }
}

#[async_trait]
impl MemoryCapability for Refusing {
    async fn store(&self, _: NoteToStore, _: ToolContext) -> Result<NoteStored, CapabilityError> {
        Err(refused())
    }
}

#[async_trait]
impl JobsCapability for Refusing {
    async fn run_workflow(
        &self,
        _: RunWorkflow,
        _: ToolContext,
    ) -> Result<JobStarted, CapabilityError> {
        Err(refused())
    }

    async fn job(
        &self,
        _: uuid::Uuid,
        _: ToolContext,
    ) -> Result<Option<JobState>, CapabilityError> {
        Err(refused())
    }
}

#[derive(Debug)]
struct Source(Arc<TinyFish>);

impl CapabilitySource for Source {
    fn http(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn HttpCapability> {
        Arc::clone(&self.0) as Arc<dyn HttpCapability>
    }

    fn model(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn ModelCapability> {
        Arc::new(Refusing)
    }

    fn memory(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn MemoryCapability> {
        Arc::new(Refusing)
    }

    fn jobs(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn JobsCapability> {
        Arc::new(Refusing)
    }
}

fn tinyfish(status: u16, body: &Value) -> Arc<TinyFish> {
    Arc::new(TinyFish {
        status,
        body: body.to_string(),
        asked: Mutex::new(Vec::new()),
    })
}

fn host(store: &Arc<MemoryStore>, config: &str, engine: &Arc<TinyFish>) -> PluginHost {
    PluginHost::new(
        Arc::clone(store) as Arc<dyn Store>,
        PluginsConfig::from_yaml(config).unwrap(),
        ToolRegistry::new(),
    )
    .with_capabilities(Arc::new(Source(Arc::clone(engine))))
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
async fn a_search_asks_tinyfish_and_answers_its_results() {
    let engine = tinyfish(
        200,
        &json!({"results": [
            {"title": "Budget passes", "url": "https://ohio.gov/b", "snippet": "It passed.", "publisher": "Ohio"},
            {"title": "Second", "url": "https://dispatch.com/2", "snippet": "More."}
        ]}),
    );
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, GRANTED, &engine);
    host.install_compiled(&SearchPlugin).await.unwrap();
    let mut session = session(host.into_registry(), &["search_web"]);

    let (status, content) = call(
        &mut session,
        "search_web",
        &json!({"query": "ohio budget", "news": true, "limit": 1}),
    )
    .await;
    assert_eq!(status, CallStatus::Ran, "{content}");
    let body: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        body["result"],
        json!({"engine": "tinyfish", "tried": [], "results": [
            {"title": "Budget passes", "url": "https://ohio.gov/b", "snippet": "It passed.", "source": "Ohio"}
        ]})
    );
    let asked = engine.asked.lock().unwrap();
    assert!(
        asked[0].contains("query=ohio%20budget") && asked[0].contains("domain_type=news"),
        "{asked:?}"
    );
}

#[tokio::test]
async fn a_failed_search_fails_and_says_what_to_do() {
    let engine = tinyfish(401, &json!({"error": "unauthorized"}));
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, GRANTED, &engine);
    host.install_compiled(&SearchPlugin).await.unwrap();
    let mut session = session(host.into_registry(), &["search_web"]);
    let (status, content) = call(&mut session, "search_web", &json!({"query": "x"})).await;
    assert_ne!(status, CallStatus::Ran, "{content}");
    assert!(content.contains("credential"), "{content}");
}

#[tokio::test]
async fn without_the_grant_it_does_not_install() {
    let store = Arc::new(MemoryStore::new(3));
    let engine = tinyfish(200, &json!({"results": []}));
    let mut host = host(&store, "version: 1\nplugins: []\n", &engine);
    let error = host.install_compiled(&SearchPlugin).await.unwrap_err();
    assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
}
