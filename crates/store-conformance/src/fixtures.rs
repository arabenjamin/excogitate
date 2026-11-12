//! Builders for the cases.

use chrono::{DateTime, TimeZone, Utc};
use protocol::{Actor, DataClass, Trust};
use serde_json::json;
use store::{
    Chunk, ChunkId, FieldKind, FilterableField, Job, JobId, JobStatus, Note, NoteId, PageRequest,
    Provenance, Record, RecordDecls, RecordId, RecordTypeDecl, RelationDecl, UnitOfWork, WriteOp,
};

pub const ITEM: &str = "test.item";
pub const OWNER: &str = "test.owner";
pub const OWNS: &str = "test.owns";

/// A fixed time, `n` seconds after the suite's epoch.
pub fn at(n: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + n, 0).unwrap()
}

pub fn page(limit: u32) -> PageRequest {
    PageRequest::new(limit, 0).unwrap()
}

pub fn provenance(data_class: DataClass, trust: Trust) -> Provenance {
    Provenance {
        data_class,
        trust,
        stored_by: Actor::new("tester"),
        stored_at: at(0),
    }
}

pub fn note(body: &str) -> Note {
    Note {
        id: NoteId::generate(),
        title: "A title".to_owned(),
        body: body.to_owned(),
        tags: vec!["one".to_owned(), "two".to_owned()],
        source_url: Some("https://example.com/page".to_owned()),
        content_hash: format!("hash-of-{body}"),
        provenance: provenance(DataClass::Private, Trust::Trusted),
        version: 1,
        updated_at: at(1),
    }
}

pub fn chunk(note_id: NoteId, ordinal: u32, text: &str, embedding: Option<[f32; 3]>) -> Chunk {
    Chunk {
        id: ChunkId::generate(),
        note_id,
        ordinal,
        text: text.to_owned(),
        token_count: 5,
        embedding: embedding.map(Vec::from),
    }
}

/// A unit that stores a note with one chunk.
pub fn note_with_text(note: &Note, text: &str, embedding: Option<[f32; 3]>) -> UnitOfWork {
    UnitOfWork::new()
        .with(WriteOp::CreateNote(note.clone()))
        .with(WriteOp::ReplaceChunks {
            note_id: note.id,
            chunks: vec![chunk(note.id, 0, text, embedding)],
        })
}

/// The job kinds a fixture worker runs.
pub fn kinds() -> Vec<String> {
    vec![JOB_KIND.to_owned()]
}

/// The kind every fixture job has.
pub const JOB_KIND: &str = "test";

pub fn job(priority: i32, run_at: i64) -> Job {
    Job {
        id: JobId::generate(),
        kind: JOB_KIND.to_owned(),
        payload: json!({ "n": 1 }),
        priority,
        status: JobStatus::Queued,
        requested_by: Actor::new("requester"),
        run_at: at(run_at),
        attempts: 0,
        max_attempts: 2,
        lease: None,
        last_error: None,
        result: None,
    }
}

pub fn decls() -> RecordDecls {
    let filterable = |name: &str, kind| FilterableField {
        name: name.to_owned(),
        kind,
    };
    RecordDecls {
        types: vec![
            RecordTypeDecl {
                name: ITEM.to_owned(),
                schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "size"],
                    "properties": {
                        "name": { "type": "string", "maxLength": 20 },
                        "size": { "type": "integer" },
                        "seen": { "type": "string", "format": "date-time" }
                    }
                }),
                filterable: vec![
                    filterable("name", FieldKind::Text),
                    filterable("size", FieldKind::Integer),
                    filterable("seen", FieldKind::Timestamp),
                ],
            },
            RecordTypeDecl {
                name: OWNER.to_owned(),
                schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "name": { "type": "string", "maxLength": 20 } }
                }),
                filterable: vec![],
            },
        ],
        relations: vec![RelationDecl {
            name: OWNS.to_owned(),
            from: OWNER.to_owned(),
            to: ITEM.to_owned(),
        }],
    }
}

pub fn record(record_type: &str, data: serde_json::Value) -> Record {
    Record::new(
        record_type.to_owned(),
        RecordId::generate(),
        data,
        provenance(DataClass::Private, Trust::Untrusted),
    )
}

pub fn item(name: &str, size: i64) -> Record {
    record(ITEM, json!({ "name": name, "size": size }))
}
