//! Applying a unit of work: every write in one transaction.

use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use neo4rs::{BoltType, Query, Txn, query};
use store::{
    Chunk, ChunkId, NoteId, Record, RecordCatalog, Store, StoreError, UnitOfWork, WriteOp,
};

use crate::codec::{
    self, at, definition_kind_name, field_param, field_property, get, int, json_text, maybe_at,
    provenance_props, task_status_name,
};
use crate::{Neo4jStore, finish, retrying, txn_rows};

#[async_trait]
impl Store for Neo4jStore {
    async fn apply(&self, unit: UnitOfWork) -> Result<(), StoreError> {
        retrying!(self.apply_once(&unit).await)
    }
}

impl Neo4jStore {
    async fn apply_once(&self, unit: &UnitOfWork) -> Result<(), StoreError> {
        let catalog = self.catalog();
        let mut txn = self.begin().await?;
        let outcome = apply_all(&mut txn, unit, &catalog, self.dimension).await;
        finish(txn, outcome).await
    }
}

async fn apply_all(
    txn: &mut Txn,
    unit: &UnitOfWork,
    catalog: &RecordCatalog,
    dimension: usize,
) -> Result<(), StoreError> {
    for op in &unit.ops {
        apply_op(txn, op, catalog, dimension).await?;
    }
    Ok(())
}

/// How many nodes or relationships a query matched, from `RETURN count(..) AS n`.
async fn count(txn: &mut Txn, query: Query) -> Result<i64, StoreError> {
    let rows = txn_rows(txn, query).await?;
    rows.first().map_or(Ok(0), |row| get(row, "n"))
}

async fn run(txn: &mut Txn, query: Query) -> Result<(), StoreError> {
    txn.run(query).await.map_err(codec::from_neo)
}

#[expect(clippy::too_many_lines, reason = "one arm per write, each short")]
async fn apply_op(
    txn: &mut Txn,
    op: &WriteOp,
    catalog: &RecordCatalog,
    dimension: usize,
) -> Result<(), StoreError> {
    match op {
        WriteOp::CreateNote(note) => {
            let id = note.id.to_string();
            let found = count(
                txn,
                query("MATCH (n:Note {id: $id}) RETURN count(n) AS n").param("id", id.as_str()),
            )
            .await?;
            if found > 0 {
                return Err(StoreError::Conflict(format!("note {id} exists")));
            }
            run(
                txn,
                query("CREATE (n:Note) SET n = $props").param("props", codec::note_props(note, 1)?),
            )
            .await?;
        }
        WriteOp::UpdateNote {
            note,
            expected_version,
        } => {
            let id = note.id.to_string();
            let rows = txn_rows(
                txn,
                query("MATCH (n:Note {id: $id}) SET n.locked = true REMOVE n.locked RETURN n.version AS version")
                    .param("id", id.as_str()),
            )
            .await?;
            let row = rows
                .first()
                .ok_or_else(|| StoreError::NotFound(format!("note {id}")))?;
            let stored: i64 = get(row, "version")?;
            if stored != int(*expected_version)? {
                return Err(StoreError::Conflict(format!(
                    "note {id} is at version {stored}, not {expected_version}"
                )));
            }
            let version = expected_version
                .checked_add(1)
                .ok_or_else(|| StoreError::Conflict(format!("note {id} has no versions left")))?;
            run(
                txn,
                query("MATCH (n:Note {id: $id}) SET n = $props")
                    .param("id", id.as_str())
                    .param("props", codec::note_props(note, version)?),
            )
            .await?;
        }
        WriteOp::DeleteNote(id) => {
            let deleted = count(
                txn,
                query(
                    "MATCH (n:Note {id: $id}) OPTIONAL MATCH (n)-[:HAS_CHUNK]->(c:Chunk) \
                     WITH n, collect(c) AS chunks FOREACH (c IN chunks | DETACH DELETE c) \
                     DETACH DELETE n RETURN count(*) AS n",
                )
                .param("id", id.to_string()),
            )
            .await?;
            if deleted == 0 {
                return Err(StoreError::NotFound(format!("note {id}")));
            }
        }
        WriteOp::ReplaceChunks { note_id, chunks } => {
            replace_chunks(txn, *note_id, chunks, dimension).await?;
        }
        WriteOp::PutTask(task) => {
            run(
                txn,
                query(
                    "MERGE (t:Task {id: $id}) SET t.title = $title, t.detail = $detail, \
                     t.status = $status, t.due_at = $due_at, t.created_at = $created_at",
                )
                .param("id", task.id.to_string())
                .param("title", task.title.as_str())
                .param("detail", task.detail.as_str())
                .param("status", task_status_name(task.status))
                .param("due_at", maybe_at(task.due_at))
                .param("created_at", at(task.created_at)),
            )
            .await?;
        }
        WriteOp::DeleteTask(id) => {
            let deleted = count(
                txn,
                query("MATCH (t:Task {id: $id}) DETACH DELETE t RETURN count(*) AS n")
                    .param("id", id.to_string()),
            )
            .await?;
            if deleted == 0 {
                return Err(StoreError::NotFound(format!("task {id}")));
            }
        }
        WriteOp::PutWorking(entry) => {
            run(
                txn,
                query(
                    "MERGE (w:Working {scope: $scope, key: $key}) \
                     SET w.value = $value, w.expires_at = $expires_at",
                )
                .param("scope", entry.scope.as_str())
                .param("key", entry.key.as_str())
                .param("value", json_text(&entry.value))
                .param("expires_at", maybe_at(entry.expires_at)),
            )
            .await?;
        }
        WriteOp::DeleteWorking { scope, key } => {
            run(
                txn,
                query("MATCH (w:Working {scope: $scope, key: $key}) DELETE w")
                    .param("scope", scope.as_str())
                    .param("key", key.as_str()),
            )
            .await?;
        }
        WriteOp::EnqueueJob(job) => {
            let id = job.id.to_string();
            let found = count(
                txn,
                query("MATCH (j:Job {id: $id}) RETURN count(j) AS n").param("id", id.as_str()),
            )
            .await?;
            if found > 0 {
                return Err(StoreError::Conflict(format!("job {id} exists")));
            }
            run(
                txn,
                query("CREATE (j:Job) SET j = $props").param("props", codec::job_props(job)?),
            )
            .await?;
        }
        WriteOp::PutDefinition(definition) => {
            run(
                txn,
                query(
                    "MERGE (d:Definition {kind: $kind, name: $name}) \
                     SET d.body = $body, d.source = $source, d.updated_at = $updated_at",
                )
                .param("kind", definition_kind_name(definition.key.kind))
                .param("name", definition.key.name.as_str())
                .param("body", json_text(&definition.body))
                .param("source", definition.source.as_str())
                .param("updated_at", at(definition.updated_at)),
            )
            .await?;
        }
        WriteOp::DeleteDefinition(key) => {
            run(
                txn,
                query("MATCH (d:Definition {kind: $kind, name: $name}) DELETE d")
                    .param("kind", definition_kind_name(key.kind))
                    .param("name", key.name.as_str()),
            )
            .await?;
        }
        WriteOp::PutRecord(record) => {
            catalog.check_record(record)?;
            run(
                txn,
                query("MERGE (r:Record {type: $type, id: $id}) SET r = $props")
                    .param("type", record.record_type.as_str())
                    .param("id", record.id.to_string())
                    .param("props", record_props(record, catalog)),
            )
            .await?;
        }
        WriteOp::DeleteRecord { record_type, id } => {
            let deleted = count(
                txn,
                query(
                    "MATCH (r:Record {type: $type, id: $id}) DETACH DELETE r RETURN count(*) AS n",
                )
                .param("type", record_type.as_str())
                .param("id", id.to_string()),
            )
            .await?;
            if deleted == 0 {
                return Err(StoreError::NotFound(format!("{record_type} {id}")));
            }
        }
        WriteOp::Link { relation, from, to } => {
            let decl = catalog.relation(relation)?;
            for (record_type, id) in [(&decl.from, from), (&decl.to, to)] {
                let found = count(
                    txn,
                    query("MATCH (r:Record {type: $type, id: $id}) RETURN count(r) AS n")
                        .param("type", record_type.as_str())
                        .param("id", id.to_string()),
                )
                .await?;
                if found == 0 {
                    return Err(StoreError::NotFound(format!("{record_type} {id}")));
                }
            }
            run(
                txn,
                query(
                    "MATCH (a:Record {type: $from_type, id: $from}) \
                     MATCH (b:Record {type: $to_type, id: $to}) \
                     MERGE (a)-[:LINK {relation: $relation}]->(b)",
                )
                .param("from_type", decl.from.as_str())
                .param("from", from.to_string())
                .param("to_type", decl.to.as_str())
                .param("to", to.to_string())
                .param("relation", relation.as_str()),
            )
            .await?;
        }
        WriteOp::Unlink { relation, from, to } => {
            let decl = catalog.relation(relation)?;
            run(
                txn,
                query(
                    "MATCH (:Record {type: $from_type, id: $from})-[l:LINK {relation: $relation}]->\
                     (:Record {type: $to_type, id: $to}) DELETE l",
                )
                .param("from_type", decl.from.as_str())
                .param("from", from.to_string())
                .param("to_type", decl.to.as_str())
                .param("to", to.to_string())
                .param("relation", relation.as_str()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn replace_chunks(
    txn: &mut Txn,
    note_id: NoteId,
    chunks: &[Chunk],
    dimension: usize,
) -> Result<(), StoreError> {
    let id = note_id.to_string();
    let found = count(
        txn,
        query("MATCH (n:Note {id: $id}) SET n.locked = true REMOVE n.locked RETURN count(n) AS n")
            .param("id", id.as_str()),
    )
    .await?;
    if found == 0 {
        return Err(StoreError::NotFound(format!("note {id}")));
    }
    let mut seen = BTreeSet::<ChunkId>::new();
    for chunk in chunks {
        if chunk.note_id != note_id {
            return Err(StoreError::Invalid(format!(
                "chunk {} belongs to another note",
                chunk.id
            )));
        }
        if chunk
            .embedding
            .as_ref()
            .is_some_and(|v| v.len() != dimension)
        {
            return Err(StoreError::Invalid(format!(
                "chunk {}: embedding must have width {dimension}",
                chunk.id
            )));
        }
        if !seen.insert(chunk.id) {
            return Err(StoreError::Conflict(format!("chunk {} repeats", chunk.id)));
        }
    }
    run(
        txn,
        query("MATCH (:Note {id: $id})-[:HAS_CHUNK]->(c:Chunk) DETACH DELETE c")
            .param("id", id.as_str()),
    )
    .await?;
    let props: Vec<HashMap<String, BoltType>> = chunks.iter().map(codec::chunk_props).collect();
    run(
        txn,
        query(
            "MATCH (n:Note {id: $id}) UNWIND $chunks AS props \
             CREATE (n)-[:HAS_CHUNK]->(c:Chunk) SET c = props",
        )
        .param("id", id.as_str())
        .param("chunks", props),
    )
    .await
}

/// A record's properties: its own, and each filterable field it has, typed,
/// so filters compare in the database.
fn record_props(record: &Record, catalog: &RecordCatalog) -> HashMap<String, BoltType> {
    let mut props = provenance_props(&record.provenance);
    props.extend([
        ("type".to_owned(), record.record_type.clone().into()),
        ("id".to_owned(), record.id.to_string().into()),
        ("data".to_owned(), json_text(&record.data).into()),
    ]);
    for (field, value) in catalog.filter_values(record) {
        props.insert(field_property(field), field_param(&value));
    }
    props
}
