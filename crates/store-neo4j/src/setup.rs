//! The constraints and indexes the store relies on, and the marker node
//! that says which version of the store wrote the database.

use neo4rs::query;
use store::StoreError;

use crate::Neo4jStore;
use crate::codec::{from_neo, get};

/// The graph shape this code writes. A database marked with a higher one
/// was written by a newer store, and this one refuses it rather than guess.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// The name of the lock node the job queue takes before it changes a job.
pub(crate) const JOBS_LOCK: &str = "jobs";

/// Each is safe to run again. A uniqueness constraint is also the index
/// its lookups use. Full-text search indexes the chunk's normalized words,
/// with no stop words, so `the` and `v3` are findable like any other word.
const STATEMENTS: &[&str] = &[
    "CREATE CONSTRAINT note_id IF NOT EXISTS FOR (n:Note) REQUIRE n.id IS UNIQUE",
    "CREATE INDEX note_hash IF NOT EXISTS FOR (n:Note) ON (n.content_hash)",
    "CREATE CONSTRAINT chunk_id IF NOT EXISTS FOR (c:Chunk) REQUIRE c.id IS UNIQUE",
    "CREATE FULLTEXT INDEX chunk_terms IF NOT EXISTS FOR (c:Chunk) ON EACH [c.terms] \
     OPTIONS {indexConfig: {`fulltext.analyzer`: 'standard-no-stop-words'}}",
    "CREATE CONSTRAINT task_id IF NOT EXISTS FOR (t:Task) REQUIRE t.id IS UNIQUE",
    "CREATE CONSTRAINT working_key IF NOT EXISTS FOR (w:Working) REQUIRE (w.scope, w.key) IS UNIQUE",
    "CREATE CONSTRAINT job_id IF NOT EXISTS FOR (j:Job) REQUIRE j.id IS UNIQUE",
    "CREATE INDEX job_status IF NOT EXISTS FOR (j:Job) ON (j.status)",
    "CREATE CONSTRAINT definition_key IF NOT EXISTS FOR (d:Definition) REQUIRE (d.kind, d.name) IS UNIQUE",
    "CREATE CONSTRAINT record_key IF NOT EXISTS FOR (r:Record) REQUIRE (r.type, r.id) IS UNIQUE",
    "CREATE CONSTRAINT spend_key IF NOT EXISTS FOR (s:Spend) REQUIRE (s.provider, s.day) IS UNIQUE",
    "CREATE CONSTRAINT lock_name IF NOT EXISTS FOR (l:Lock) REQUIRE l.name IS UNIQUE",
    "CREATE CONSTRAINT store_meta IF NOT EXISTS FOR (m:StoreMeta) REQUIRE m.name IS UNIQUE",
];

pub(crate) async fn prepare(store: &Neo4jStore) -> Result<(), StoreError> {
    for statement in STATEMENTS {
        store.graph.run(query(statement)).await.map_err(from_neo)?;
    }
    // Index creation returns before the index is online; a search before
    // then would fail.
    store
        .graph
        .run(query("CALL db.awaitIndexes(300)"))
        .await
        .map_err(from_neo)?;
    store
        .graph
        .run(query("MERGE (l:Lock {name: $name})").param("name", JOBS_LOCK))
        .await
        .map_err(from_neo)?;
    let dimension = i64::try_from(store.dimension)
        .map_err(|_| StoreError::Invalid("the embedding width is too large".to_owned()))?;
    let rows = store
        .rows(
            query(
                "MERGE (m:StoreMeta {name: 'store'}) \
                 ON CREATE SET m.schema_version = $version, m.embedding_dimension = $dimension \
                 RETURN m.schema_version AS version, m.embedding_dimension AS dimension",
            )
            .param("version", SCHEMA_VERSION)
            .param("dimension", dimension),
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| StoreError::Backend("the store's marker wasn't written".to_owned()))?;
    let (version, stored): (i64, i64) = (get(row, "version")?, get(row, "dimension")?);
    if version > SCHEMA_VERSION {
        return Err(StoreError::Invalid(format!(
            "the database was written by a newer store (schema {version}; this one knows {SCHEMA_VERSION})"
        )));
    }
    if stored != dimension {
        return Err(StoreError::Invalid(format!(
            "the database holds embeddings of width {stored}, and the store was opened for {dimension}"
        )));
    }
    Ok(())
}
