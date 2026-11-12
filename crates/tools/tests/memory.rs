//! The `memory` tool, over an in-memory store with no embedder.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use memory::{
    ChunkConfig, EmbedError, EmbedScope, Embedder, Embedding, Memory, NewNote, NoteOrigin,
};
use plugin_api::{Effect, Tool, ToolContext, ToolFailure, ToolOutput};
use protocol::{Actor, DataClass, RunId, Trust};
use schema::{Validator, check_strict};
use serde_json::{Value, json};
use store::{NoteId, Store};
use store_memory::MemoryStore;
use tools::MemoryTool;
use uuid::Uuid;

fn memory() -> Memory {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(3));
    Memory::new(store, None, ChunkConfig::default())
}

fn context(data_class: DataClass) -> ToolContext {
    ToolContext::new(RunId::generate(), data_class, 1000)
}

fn private() -> ToolContext {
    context(DataClass::Private)
}

fn public() -> ToolContext {
    context(DataClass::Public)
}

async fn call(
    tool: &MemoryTool,
    args: Value,
    context: ToolContext,
) -> Result<ToolOutput, ToolFailure> {
    let definition = tool.definition();
    Validator::from_schema(&definition.input_schema)
        .unwrap()
        .check(&args)
        .unwrap();
    let output = tool.run(args, context).await?;
    Validator::from_schema(&definition.output_schema)
        .unwrap()
        .check(&output.value)
        .unwrap();
    Ok(output)
}

async fn stored_id(tool: &MemoryTool, text: &str, context: ToolContext) -> String {
    let output = call(tool, json!({ "operation": "store", "text": text }), context)
        .await
        .unwrap();
    output.value["id"].as_str().unwrap().to_owned()
}

/// A note from the owner, the way the runtime's own code would write it.
async fn owners_note(memory: &Memory) -> NoteId {
    memory
        .store(
            NewNote {
                title: "mine".to_owned(),
                body: "the owner wrote this".to_owned(),
                tags: Vec::new(),
                source_url: None,
            },
            NoteOrigin {
                data_class: DataClass::Private,
                trust: Trust::Trusted,
                stored_by: Actor::new("owner"),
            },
            None,
            Utc::now(),
        )
        .await
        .unwrap()
        .id
}

/// Costs 9 a call, and remembers who each call was for.
#[derive(Debug, Default)]
struct Metered {
    scopes: Mutex<Vec<EmbedScope>>,
}

#[async_trait]
impl Embedder for Metered {
    fn dimension(&self) -> usize {
        3
    }

    async fn embed(&self, inputs: &[String], scope: EmbedScope) -> Result<Embedding, EmbedError> {
        self.scopes.lock().unwrap().push(scope);
        Ok(Embedding {
            vectors: inputs.iter().map(|_| vec![1.0, 0.0, 0.0]).collect(),
            cost_usd_micros: 9,
            model: None,
        })
    }
}

#[tokio::test]
async fn embedding_is_paid_for_by_the_run() {
    let metered = Arc::new(Metered::default());
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(3));
    let tool = MemoryTool::new(Memory::new(
        store,
        Some(metered.clone() as Arc<dyn Embedder>),
        ChunkConfig::default(),
    ));
    let wrote = call(
        &tool,
        json!({ "operation": "store", "text": "otters" }),
        public(),
    )
    .await
    .unwrap();
    assert_eq!(wrote.cost_usd_micros, 9);
    let id = wrote.value["id"].clone();
    let found = call(
        &tool,
        json!({ "operation": "search", "query": "otters" }),
        public(),
    )
    .await
    .unwrap();
    assert_eq!(found.cost_usd_micros, 9);
    let changed = call(
        &tool,
        json!({ "operation": "update", "id": id, "text": "beavers" }),
        public(),
    )
    .await
    .unwrap();
    assert_eq!(changed.cost_usd_micros, 9);
    let want = EmbedScope {
        data_class: DataClass::Public,
        budget_usd_micros: Some(1000),
    };
    assert_eq!(*metered.scopes.lock().unwrap(), vec![want, want, want]);
}

#[tokio::test]
async fn min_similarity_keeps_only_close_notes() {
    let metered = Arc::new(Metered::default());
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(3));
    let with_vectors = MemoryTool::new(Memory::new(
        store,
        Some(metered as Arc<dyn Embedder>),
        ChunkConfig::default(),
    ));
    stored_id(&with_vectors, "otters", private()).await;
    let found =
        |min: f64| json!({ "operation": "search", "query": "otters", "min_similarity": min });
    let close = call(&with_vectors, found(0.99), private()).await.unwrap();
    assert_eq!(close.value["hits"].as_array().unwrap().len(), 1);
    assert!(close.value["hits"][0]["similarity"].as_f64().unwrap() > 0.99);

    // Found by keyword alone, so there's no similarity to go on.
    let no_vectors = MemoryTool::new(memory());
    stored_id(&no_vectors, "otters", private()).await;
    let filtered = call(&no_vectors, found(0.1), private()).await.unwrap();
    assert_eq!(filtered.value["hits"].as_array().unwrap().len(), 0);
    let unfiltered = call(
        &no_vectors,
        json!({ "operation": "search", "query": "otters" }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(unfiltered.value["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn its_schemas_are_strict() {
    let definition = MemoryTool::new(memory()).definition();
    assert_eq!(check_strict(&definition.input_schema), vec![]);
    assert_eq!(check_strict(&definition.output_schema), vec![]);
    assert_eq!(definition.input_schema["type"], "object");
}

#[test]
fn each_operation_requires_its_own_fields() {
    let definition = MemoryTool::new(memory()).definition();
    let validator = Validator::from_schema(&definition.input_schema).unwrap();
    let id = Uuid::nil();
    for good in [
        json!({ "operation": "store", "text": "a" }),
        json!({ "operation": "search", "query": "a" }),
        json!({ "operation": "get", "id": id }),
        json!({ "operation": "delete", "id": id }),
        json!({ "operation": "update", "id": id, "title": "b" }),
    ] {
        assert!(validator.check(&good).is_ok(), "{good} was refused");
    }
    for bad in [
        json!({ "operation": "store" }),
        json!({ "operation": "store", "text": null }),
        json!({ "operation": "search" }),
        json!({ "operation": "get" }),
        json!({ "operation": "delete", "id": null }),
        json!({ "operation": "update", "title": "b" }),
        json!({ "operation": "store", "text": "a", "trust": "trusted" }),
        json!({ "operation": "store", "text": "a", "data_class": "public" }),
        json!({ "operation": "drop_everything" }),
    ] {
        assert!(validator.check(&bad).is_err(), "{bad} was accepted");
    }
}

#[test]
fn only_changes_count_as_writes() {
    let tool = MemoryTool::new(memory());
    for (operation, want) in [
        ("search", Effect::Read),
        ("get", Effect::Read),
        ("store", Effect::Write),
        ("update", Effect::Write),
        ("delete", Effect::Write),
    ] {
        assert_eq!(tool.effect_of(&json!({ "operation": operation })), want);
    }
    assert_eq!(tool.definition().effect, Effect::Write);
}

#[tokio::test]
async fn it_stores_finds_changes_and_forgets() {
    let tool = MemoryTool::new(memory());
    let id = stored_id(&tool, "Otters hold hands\nwhile they sleep", private()).await;

    let found = call(
        &tool,
        json!({ "operation": "search", "query": "otters" }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(found.value["hits"][0]["id"], id);
    assert_eq!(found.value["hits"][0]["title"], "Otters hold hands");
    assert_eq!(found.value["hits"][0]["trust"], "untrusted");
    assert!(found.value["keyword_only_because"].is_string());
    assert_eq!(found.trust, Trust::Untrusted);

    let changed = call(
        &tool,
        json!({ "operation": "update", "id": id, "text": "Beavers build dams" }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(changed.value["created"], false);
    let got = call(&tool, json!({ "operation": "get", "id": id }), private())
        .await
        .unwrap();
    assert_eq!(got.value["text"], "Beavers build dams");
    assert_eq!(got.trust, Trust::Untrusted);

    call(&tool, json!({ "operation": "delete", "id": id }), private())
        .await
        .unwrap();
    let gone = call(&tool, json!({ "operation": "get", "id": id }), private())
        .await
        .unwrap_err();
    assert!(gone.message().contains("no note"), "{gone}");
}

#[tokio::test]
async fn an_update_with_nothing_to_change_fails() {
    let tool = MemoryTool::new(memory());
    let id = stored_id(&tool, "text", private()).await;
    let error = call(&tool, json!({ "operation": "update", "id": id }), private())
        .await
        .unwrap_err();
    assert!(error.message().contains("nothing to change"), "{error}");
}

#[tokio::test]
async fn what_a_model_stores_is_untrusted_and_in_its_runs_class() {
    let memory = memory();
    let tool = MemoryTool::new(memory.clone());
    let id = stored_id(&tool, "something", public()).await;
    let note = memory
        .get(
            NoteId::from_uuid(id.parse().unwrap()),
            store::ClassFilter::Any,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.provenance.trust, Trust::Untrusted);
    assert_eq!(note.provenance.data_class, DataClass::Public);
    assert!(note.provenance.stored_by.as_str().starts_with("run:"));
}

#[tokio::test]
async fn a_public_run_cant_see_or_touch_private_notes() {
    let tool = MemoryTool::new(memory());
    let id = stored_id(&tool, "a private otter", private()).await;

    let found = call(
        &tool,
        json!({ "operation": "search", "query": "otter" }),
        public(),
    )
    .await
    .unwrap();
    assert_eq!(found.value["hits"].as_array().unwrap().len(), 0);
    for args in [
        json!({ "operation": "get", "id": id }),
        json!({ "operation": "delete", "id": id }),
        json!({ "operation": "update", "id": id, "title": "x" }),
    ] {
        assert!(call(&tool, args, public()).await.is_err());
    }
    assert!(
        call(&tool, json!({ "operation": "get", "id": id }), private())
            .await
            .is_ok(),
        "it's still there"
    );
}

#[tokio::test]
async fn the_owners_notes_cant_be_changed_or_deleted() {
    let memory = memory();
    let id = owners_note(&memory).await;
    let tool = MemoryTool::new(memory.clone());
    for args in [
        json!({ "operation": "delete", "id": id.as_uuid() }),
        json!({ "operation": "update", "id": id.as_uuid(), "text": "rewritten" }),
    ] {
        let error = call(&tool, args, private()).await.unwrap_err();
        assert!(error.message().contains("owner"), "{error}");
    }
    let note = memory
        .get(id, store::ClassFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.body, "the owner wrote this");
}

#[tokio::test]
async fn results_of_only_the_owners_notes_are_trusted() {
    let memory = memory();
    let id = owners_note(&memory).await;
    let tool = MemoryTool::new(memory);
    let got = call(
        &tool,
        json!({ "operation": "get", "id": id.as_uuid() }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(got.trust, Trust::Trusted);
    let found = call(
        &tool,
        json!({ "operation": "search", "query": "owner" }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(found.trust, Trust::Trusted);
    stored_id(&tool, "the owner of this otter", private()).await;
    let mixed = call(
        &tool,
        json!({ "operation": "search", "query": "owner" }),
        private(),
    )
    .await
    .unwrap();
    assert_eq!(mixed.trust, Trust::Untrusted);
}

/// A source with one item about everything.
#[derive(Debug)]
struct OneItem;

#[async_trait]
impl plugin_api::Source for OneItem {
    fn name(&self) -> &'static str {
        "one"
    }

    async fn search(
        &self,
        _: plugin_api::SourceQuery,
        _: ToolContext,
    ) -> Result<plugin_api::SourceFound, ToolFailure> {
        let item = plugin_api::SourceItem::new("i", "Item", "about otters");
        Ok(plugin_api::SourceFound::new(vec![item]))
    }

    async fn get(&self, _: &str, _: ToolContext) -> Result<plugin_api::SourceFound, ToolFailure> {
        Ok(plugin_api::SourceFound::default())
    }
}

#[tokio::test]
async fn a_run_limited_to_no_sources_reads_none() {
    let memory = memory();
    memory
        .attach_sources(vec![memory::LakeSource {
            id: "demo.one".to_owned(),
            public: false,
            words: true,
            vectors_from: std::collections::BTreeSet::new(),
            dimension: None,
            time_limit: std::time::Duration::from_secs(1),
            source: Arc::new(OneItem),
            meter: None,
        }])
        .unwrap();
    let choices = memory::SourceChoices::default();
    let tool = MemoryTool::new(memory).with_choices(choices.clone());
    let search = json!({ "operation": "search", "query": "otters" });

    let open_run = private();
    let found = call(&tool, search.clone(), open_run).await.unwrap();
    assert_eq!(found.value["sources"][0]["source"], "demo.one");
    assert_eq!(found.trust, Trust::Untrusted);

    let narrow_run = private();
    let held = choices.choose(narrow_run.run, &[]);
    let found = call(&tool, search.clone(), narrow_run).await.unwrap();
    assert_eq!(found.value["sources"], json!([]));
    drop(held);
    let found = call(&tool, search, narrow_run).await.unwrap();
    assert_eq!(
        found.value["sources"][0]["id"], "i",
        "the choice ends with its run"
    );
}
