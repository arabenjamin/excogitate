//! Export and import: everything a store holds goes out and comes back the
//! same, and an import refuses what it can't load whole.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use protocol::{Actor, DataClass, Trust};
use runtime::{ArchiveError, Source, export, import};
use serde_json::json;
use store::{
    Archive, Chunk, ChunkId, Definition, DefinitionKey, DefinitionKind, DefinitionReader,
    FieldKind, FilterableField, Job, JobId, JobQueue, JobStatus, Note, NoteId, NoteReader,
    PageRequest, Provenance, Record, RecordDecls, RecordId, RecordReader, RecordTypeDecl,
    RelationDecl, SpendLog, Store, Task, TaskId, TaskQuery, TaskReader, TaskStatus, UnitOfWork,
    WorkingEntry, WriteOp,
};
use store_memory::MemoryStore;

const WIDTH: usize = 3;

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + seconds, 0).unwrap()
}

fn page() -> PageRequest {
    PageRequest::new(100, 0).unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("archive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn provenance() -> Provenance {
    Provenance {
        data_class: DataClass::Public,
        trust: Trust::Untrusted,
        stored_by: Actor::new("tester"),
        stored_at: at(0),
    }
}

fn decls() -> RecordDecls {
    let open = |name: &str| RecordTypeDecl {
        name: name.to_owned(),
        schema: json!({
            "type": "object", "additionalProperties": false,
            "properties": { "name": { "type": "string", "maxLength": 20 } }
        }),
        filterable: vec![FilterableField {
            name: "name".to_owned(),
            kind: FieldKind::Text,
        }],
    };
    RecordDecls {
        types: vec![open("test.owner"), open("test.item")],
        relations: vec![RelationDecl {
            name: "test.owns".to_owned(),
            from: "test.owner".to_owned(),
            to: "test.item".to_owned(),
        }],
    }
}

fn job(status: JobStatus, finished: Option<i64>) -> Job {
    Job {
        id: JobId::generate(),
        kind: "test".to_owned(),
        payload: json!({ "n": 1 }),
        priority: 2,
        status,
        requested_by: Actor::new("owner"),
        run_at: at(3),
        attempts: u32::from(finished.is_some()),
        max_attempts: 3,
        lease: None,
        last_error: None,
        result: finished.map(|_| json!({ "answer": 42 })),
        finished_at: finished.map(at),
    }
}

/// A store with some of everything, and what's in it.
async fn filled() -> MemoryStore {
    let store = MemoryStore::new(WIDTH);
    store.register_records(&decls()).await.unwrap();
    let note = Note {
        id: NoteId::generate(),
        title: "Otters".to_owned(),
        body: "Otters hold hands.".to_owned(),
        tags: vec!["animals".to_owned()],
        source_url: Some("https://example.com/otters".to_owned()),
        content_hash: "h".to_owned(),
        provenance: provenance(),
        version: 1,
        updated_at: at(1),
    };
    let chunks = (0..2)
        .map(|ordinal| Chunk {
            id: ChunkId::generate(),
            note_id: note.id,
            ordinal,
            text: format!("part {ordinal}"),
            token_count: 2,
            embedding: Some(vec![0.5, 0.25, 1.0]),
        })
        .collect();
    let owner = Record::new(
        "test.owner".to_owned(),
        RecordId::generate(),
        json!({ "name": "o" }),
        provenance(),
    );
    let item = Record::new(
        "test.item".to_owned(),
        RecordId::generate(),
        json!({ "name": "i" }),
        provenance(),
    );
    let unit = UnitOfWork::new()
        .with(WriteOp::CreateNote(note.clone()))
        .with(WriteOp::ReplaceChunks {
            note_id: note.id,
            chunks,
        })
        .with(WriteOp::PutTask(Task {
            id: TaskId::generate(),
            title: "Feed the otters".to_owned(),
            detail: String::new(),
            status: TaskStatus::Pending,
            due_at: Some(at(50)),
            created_at: at(2),
        }))
        .with(WriteOp::PutWorking(WorkingEntry {
            scope: "chat".to_owned(),
            key: "kept".to_owned(),
            value: json!([1, 2]),
            expires_at: None,
        }))
        .with(WriteOp::PutWorking(WorkingEntry {
            scope: "chat".to_owned(),
            key: "gone".to_owned(),
            value: json!(null),
            expires_at: Some(at(1)),
        }))
        .with(WriteOp::RestoreJob(job(JobStatus::Succeeded, Some(9))))
        .with(WriteOp::EnqueueJob(job(JobStatus::Queued, None)))
        .with(WriteOp::PutDefinition(Definition {
            key: DefinitionKey {
                kind: DefinitionKind::Agent,
                name: "helper".to_owned(),
            },
            body: json!({ "system_prompt": "You help." }),
            source: "agents/helper.yaml".to_owned(),
            updated_at: at(4),
        }))
        .with(WriteOp::PutRecord(owner.clone()))
        .with(WriteOp::PutRecord(item.clone()))
        .with(WriteOp::Link {
            relation: "test.owns".to_owned(),
            from: owner.id,
            to: item.id,
        });
    store.apply(unit).await.unwrap();
    assert!(
        store
            .reserve_spend(
                "cloud",
                NaiveDate::from_ymd_opt(2027, 1, 2).unwrap(),
                1234,
                10_000
            )
            .await
            .unwrap()
    );
    store
}

async fn exported(store: &MemoryStore, name: &str) -> PathBuf {
    let directory = scratch(name);
    export(
        Source {
            store,
            store_name: "memory",
            runtime: "test 0.0.0",
            now: at(10),
        },
        &directory,
    )
    .await
    .unwrap();
    directory
}

async fn ready() -> MemoryStore {
    let store = MemoryStore::new(WIDTH);
    store.register_records(&decls()).await.unwrap();
    store
}

#[tokio::test]
async fn everything_comes_back_the_same() {
    let from = filled().await;
    let directory = exported(&from, "round").await;
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(directory.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["format_version"], 1);
    assert_eq!(manifest["embedding_dimension"], WIDTH);
    assert_eq!(
        manifest["counts"],
        json!({ "notes": 1, "chunks": 2, "tasks": 1, "working": 1, "jobs": 2,
                "definitions": 1, "records": 2, "links": 1, "spend": 1 })
    );

    let into = ready().await;
    let counts = import(&into, &directory).await.unwrap();
    assert_eq!(counts.notes, 1);
    assert_eq!(
        into.list_notes(page()).await.unwrap().items,
        from.list_notes(page()).await.unwrap().items
    );
    let note = from.list_notes(page()).await.unwrap().items[0].id;
    assert_eq!(
        into.list_chunks(note).await.unwrap(),
        from.list_chunks(note).await.unwrap()
    );
    assert_eq!(
        into.list_tasks(TaskQuery::default(), page())
            .await
            .unwrap()
            .items,
        from.list_tasks(TaskQuery::default(), page())
            .await
            .unwrap()
            .items
    );
    assert_eq!(
        into.list_working(at(10), page()).await.unwrap().items,
        from.list_working(at(10), page()).await.unwrap().items
    );
    // Jobs come back as they were: the finished one isn't queued again.
    assert_eq!(
        into.list_jobs(None, page()).await.unwrap().items,
        from.list_jobs(None, page()).await.unwrap().items
    );
    assert_eq!(
        into.list_definitions(DefinitionKind::Agent, page())
            .await
            .unwrap()
            .items,
        from.list_definitions(DefinitionKind::Agent, page())
            .await
            .unwrap()
            .items
    );
    assert_eq!(
        into.list_all_records(page()).await.unwrap().items,
        from.list_all_records(page()).await.unwrap().items
    );
    assert_eq!(
        into.list_links(page()).await.unwrap().items,
        from.list_links(page()).await.unwrap().items
    );
    assert_eq!(
        into.list_spend(page()).await.unwrap().items,
        from.list_spend(page()).await.unwrap().items
    );
}

fn refusal(result: Result<runtime::Counts, ArchiveError>) -> String {
    result.unwrap_err().to_string()
}

#[tokio::test]
async fn an_export_needs_a_new_directory() {
    let store = filled().await;
    let directory = exported(&store, "twice").await;
    let again = export(
        Source {
            store: &store,
            store_name: "memory",
            runtime: "test",
            now: at(10),
        },
        &directory,
    )
    .await
    .unwrap_err();
    assert!(again.to_string().contains("isn't empty"), "{again}");
}

#[tokio::test]
async fn an_import_goes_only_into_an_empty_store() {
    let directory = exported(&filled().await, "nonempty").await;
    let busy = filled().await;
    let why = refusal(import(&busy, &directory).await);
    assert!(
        why.contains("isn't empty") && why.contains("notes"),
        "{why}"
    );
}

#[tokio::test]
async fn records_no_plugin_declares_are_refused_before_anything_is_written() {
    let directory = exported(&filled().await, "undeclared").await;
    let bare = MemoryStore::new(WIDTH);
    let why = refusal(import(&bare, &directory).await);
    assert!(
        why.contains("record type test.item") && why.contains("relation test.owns"),
        "{why}"
    );
    assert_eq!(bare.list_notes(page()).await.unwrap().items, vec![]);
}

#[tokio::test]
async fn embeddings_of_another_width_are_refused() {
    let directory = exported(&filled().await, "width").await;
    let wider = MemoryStore::new(WIDTH + 1);
    wider.register_records(&decls()).await.unwrap();
    let why = refusal(import(&wider, &directory).await);
    assert!(why.contains("3 wide") && why.contains("4 wide"), "{why}");
}

fn rewrite(directory: &Path, file: &str, change: impl Fn(String) -> String) {
    let path = directory.join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, change(text)).unwrap();
}

#[tokio::test]
async fn a_bad_line_is_named_by_file_and_line() {
    let directory = exported(&filled().await, "bad-line").await;
    rewrite(&directory, "tasks.jsonl", |text| {
        text.replace("\"pending\"", "\"someday\"")
    });
    let into = ready().await;
    let why = refusal(import(&into, &directory).await);
    assert!(why.starts_with("tasks.jsonl line 1:"), "{why}");
    assert_eq!(into.list_notes(page()).await.unwrap().items, vec![]);
}

#[tokio::test]
async fn a_file_that_doesnt_match_the_manifest_is_refused() {
    let directory = exported(&filled().await, "count").await;
    rewrite(&directory, "links.jsonl", |_| String::new());
    let why = refusal(import(&ready().await, &directory).await);
    assert!(why.contains("links.jsonl has 0 lines"), "{why}");
}

#[tokio::test]
async fn an_unfinished_or_newer_export_is_refused() {
    let directory = exported(&filled().await, "manifest").await;
    rewrite(&directory, "manifest.json", |text| {
        text.replace("\"format_version\": 1", "\"format_version\": 2")
    });
    let why = refusal(import(&ready().await, &directory).await);
    assert!(why.contains("version 2"), "{why}");
    std::fs::remove_file(directory.join("manifest.json")).unwrap();
    let why = refusal(import(&ready().await, &directory).await);
    assert!(why.contains("didn't finish"), "{why}");
}
