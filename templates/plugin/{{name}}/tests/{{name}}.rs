{%- set caps = capabilities | split(pat=",") -%}
{%- if tools %}{% set tool_list = tools | split(pat=",") %}{% else %}{% set tool_list = [] %}{% endif -%}
{%- if records %}{% set record_list = records | split(pat=",") %}{% else %}{% set record_list = [] %}{% endif -%}
{%- set outside = [] -%}
{%- for capability in ["http", "model", "memory", "jobs"] -%}
{%- if capability in caps %}{% set_global outside = outside | concat(with=capability) %}{% endif -%}
{%- endfor -%}
{%- set kind = name | title -%}
//! {{ name }} through the real host and dispatcher, on the memory store.
//! Keep these, and add one for each thing a tool really does.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::Arc;
{% if outside %}
use async_trait::async_trait;
{%- endif %}
use dispatch::{Budget, CallStatus, Session, SessionConfig, ToolRegistry};
use events::MemorySink;
use model::ToolCall;
use plugin_api::{
    DataClass, RunId,
{%- if outside %} CapabilityError, HttpCapability, HttpRequest, HttpResponse, JobStarted, JobState, JobsCapability, MemoryCapability, ModelCapability, ModelReply, NoteStored, NoteToStore, RunWorkflow, ToolContext,{% endif %}
{%- if record_list %} PageRequest, RecordQuery,{% endif %}
};
use plugin_host::{
{%- if outside %}CapabilityMeter, CapabilitySource, {% endif %}{% if capabilities %}HostError, {% endif %}PluginHost, PluginsConfig};
use plugin_{{ name }}::{{ kind }}Plugin;
use serde_json::{Value, json};
use store::{
{%- if record_list %}RecordReader, {% endif %}Store};
use store_memory::MemoryStore;

const GRANTED: &str =
    "version: 1\nplugins:\n  - {name: {{ name }}, capabilities: [{{ capabilities | replace(from=",", to=", ") }}]}\n";

const BUDGET: Budget = Budget {
    max_rounds: 3,
    max_tool_calls: 20,
    max_writes: 10,
    max_total_tokens: 10_000,
    max_usd_micros: 0,
    max_request_bytes: 100_000,
    max_duration_ms: 20_000,
};
{%- if outside %}

/// Stands in for the runtime's capabilities, refusing every call: replace
/// it with a scripted one where a test needs answers.
#[derive(Debug)]
struct Refusing;

fn refused() -> CapabilityError {
    CapabilityError::Refused("not in this test".to_owned())
}

#[async_trait]
impl HttpCapability for Refusing {
    async fn request(&self, _: HttpRequest, _: ToolContext) -> Result<HttpResponse, CapabilityError> {
        Err(refused())
    }
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
    async fn run_workflow(&self, _: RunWorkflow, _: ToolContext) -> Result<JobStarted, CapabilityError> {
        Err(refused())
    }

    async fn job(&self, _: uuid::Uuid, _: ToolContext) -> Result<Option<JobState>, CapabilityError> {
        Err(refused())
    }
}

impl CapabilitySource for Refusing {
    fn http(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn HttpCapability> {
        Arc::new(Self)
    }

    fn model(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn ModelCapability> {
        Arc::new(Self)
    }

    fn memory(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn MemoryCapability> {
        Arc::new(Self)
    }

    fn jobs(&self, _: &str, _: Arc<CapabilityMeter>) -> Arc<dyn JobsCapability> {
        Arc::new(Self)
    }
}
{%- endif %}

fn host(store: &Arc<MemoryStore>, config: &str) -> PluginHost {
    PluginHost::new(
        Arc::clone(store) as Arc<dyn Store>,
        PluginsConfig::from_yaml(config).unwrap(),
        ToolRegistry::new(),
    )
{%- if outside %}
    .with_capabilities(Arc::new(Refusing))
{%- endif %}
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
async fn it_installs_and_each_tool_answers() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, GRANTED);
    host.install_compiled(&{{ kind }}Plugin).await.unwrap();
    let tools: &[&str] = &[{% for tool in tool_list %}"{{ name }}_{{ tool }}"{% if not loop.last %}, {% endif %}{% endfor %}];
    let mut session = session(host.into_registry(), tools);
    for &tool in tools {
        let (status, content) = call(&mut session, tool, &json!({})).await;
        assert_eq!(status, CallStatus::Ran, "{tool}: {content}");
    }
{%- for record in record_list %}
    let found = store
        .query_records(RecordQuery::new("{{ name }}.{{ record }}", PageRequest::new(5, 0).unwrap()))
        .await
        .unwrap();
    assert_eq!(found.items.len(), 0, "{{ name }}.{{ record }} is declared");
{%- endfor %}
}

{%- if capabilities %}

#[tokio::test]
async fn without_the_grant_it_does_not_install() {
    let store = Arc::new(MemoryStore::new(3));
    let mut host = host(&store, "version: 1\nplugins: []\n");
    let error = host.install_compiled(&{{ kind }}Plugin).await.unwrap_err();
    assert!(matches!(error, HostError::NotGranted { .. }), "{error}");
}
{%- endif %}
