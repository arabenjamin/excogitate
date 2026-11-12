//! Opening a database: settings, extensions and tables.

use std::path::Path;

use duckdb::{Connection, params};

use crate::OpenError;

/// The layout this code reads and writes.
const SCHEMA_VERSION: &str = "1";

/// What's ignored between tokens. The default also drops digits, which loses
/// years and version numbers.
const FTS_IGNORE: &str = r"(\.|[^a-z0-9])+";

/// Timestamps are `TIMESTAMP` holding UTC: a `DateTime<Utc>` is the only thing
/// ever bound to them, and `TIMESTAMPTZ` would need the ICU extension.
const TABLES: &str = "
CREATE TABLE notes (
    id VARCHAR NOT NULL, title VARCHAR NOT NULL, body VARCHAR NOT NULL, tags VARCHAR NOT NULL,
    source_url VARCHAR, content_hash VARCHAR NOT NULL, data_class VARCHAR NOT NULL,
    trust VARCHAR NOT NULL, stored_by VARCHAR NOT NULL, stored_at TIMESTAMP NOT NULL,
    version BIGINT NOT NULL, updated_at TIMESTAMP NOT NULL);
CREATE TABLE chunks (
    id VARCHAR NOT NULL, note_id VARCHAR NOT NULL, ordinal INTEGER NOT NULL,
    text VARCHAR NOT NULL, token_count INTEGER NOT NULL);
CREATE TABLE tasks (
    id VARCHAR NOT NULL, title VARCHAR NOT NULL, detail VARCHAR NOT NULL,
    status VARCHAR NOT NULL, due_at TIMESTAMP, created_at TIMESTAMP NOT NULL);
CREATE TABLE working (
    scope VARCHAR NOT NULL, key VARCHAR NOT NULL, value VARCHAR NOT NULL, expires_at TIMESTAMP);
CREATE TABLE jobs (
    id VARCHAR NOT NULL, kind VARCHAR NOT NULL, payload VARCHAR NOT NULL, priority INTEGER NOT NULL,
    status VARCHAR NOT NULL, requested_by VARCHAR NOT NULL, run_at TIMESTAMP NOT NULL,
    attempts INTEGER NOT NULL, max_attempts INTEGER NOT NULL, lease_worker VARCHAR,
    lease_until TIMESTAMP, last_error VARCHAR);
CREATE TABLE definitions (
    kind VARCHAR NOT NULL, name VARCHAR NOT NULL, body VARCHAR NOT NULL, source VARCHAR NOT NULL,
    updated_at TIMESTAMP NOT NULL);
CREATE TABLE records (
    record_type VARCHAR NOT NULL, id VARCHAR NOT NULL, data VARCHAR NOT NULL,
    data_class VARCHAR NOT NULL, trust VARCHAR NOT NULL, stored_by VARCHAR NOT NULL,
    stored_at TIMESTAMP NOT NULL);
CREATE TABLE links (relation VARCHAR NOT NULL, from_id VARCHAR NOT NULL, to_id VARCHAR NOT NULL);
";

/// What each provider has spent per day, and what each job produced. Run on
/// every open, so a file from before these existed gets them without a
/// migration.
const SPEND_TABLE: &str = "CREATE TABLE IF NOT EXISTS spend (
    provider VARCHAR NOT NULL, day DATE NOT NULL, usd_micros UBIGINT NOT NULL);
    ALTER TABLE jobs ADD COLUMN IF NOT EXISTS result VARCHAR;
    ALTER TABLE jobs ADD COLUMN IF NOT EXISTS finished_at TIMESTAMP;";

fn quoted(text: &str) -> String {
    text.replace('\'', "''")
}

/// Applies the settings and loads what's needed. Nothing is ever downloaded.
fn configure(connection: &Connection, extension_dir: &Path) -> Result<(), OpenError> {
    let directory = quoted(&extension_dir.to_string_lossy());
    connection
        .execute_batch(&format!(
            "SET extension_directories = ['{directory}'];
             SET autoinstall_known_extensions = false;
             SET autoload_known_extensions = false;
             SET allow_community_extensions = false;"
        ))
        .map_err(|error| OpenError::Database(error.to_string()))?;
    connection.execute_batch("LOAD fts;").map_err(|error| {
        OpenError::Extension(format!(
            "couldn't load the fts extension from {}: {error}",
            extension_dir.display()
        ))
    })
}

fn meta(connection: &Connection, key: &str) -> Result<Option<String>, OpenError> {
    let found = connection.query_row(
        "SELECT value FROM meta WHERE key = ?",
        params![key],
        |row| row.get::<_, String>(0),
    );
    match found {
        Ok(value) => Ok(Some(value)),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(OpenError::Database(error.to_string())),
    }
}

fn is_new(connection: &Connection) -> Result<bool, OpenError> {
    connection
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'meta'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count == 0)
        .map_err(|error| OpenError::Database(error.to_string()))
}

fn create(connection: &Connection, dimension: usize) -> Result<(), OpenError> {
    connection
        .execute_batch(&format!(
            "BEGIN;
             CREATE TABLE meta (key VARCHAR NOT NULL, value VARCHAR NOT NULL);
             {TABLES}
             CREATE TABLE chunk_vectors (chunk_id VARCHAR NOT NULL, embedding FLOAT[{dimension}] NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '{SCHEMA_VERSION}'),
                                     ('embedding_dimension', '{dimension}');
             COMMIT;
             PRAGMA create_fts_index('chunks', 'id', 'text', overwrite = 1, stemmer = 'none',
                 stopwords = 'none', lower = 1, strip_accents = 1,
                 ignore = '{FTS_IGNORE}', incremental = 1);"
        ))
        .map_err(|error| OpenError::Database(error.to_string()))
}

/// Prepares a connection, creating the tables on first use and refusing a
/// file this code can't read.
pub(crate) fn prepare(
    connection: &Connection,
    extension_dir: &Path,
    dimension: usize,
) -> Result<(), OpenError> {
    configure(connection, extension_dir)?;
    if is_new(connection)? {
        create(connection, dimension)?;
    } else {
        check_existing(connection, dimension)?;
    }
    connection
        .execute_batch(SPEND_TABLE)
        .map_err(|error| OpenError::Database(error.to_string()))
}

/// Refuses a file this code can't read.
fn check_existing(connection: &Connection, dimension: usize) -> Result<(), OpenError> {
    let version = meta(connection, "schema_version")?;
    if version.as_deref() != Some(SCHEMA_VERSION) {
        return Err(OpenError::Incompatible(format!(
            "the file has schema version {version:?}; this build reads {SCHEMA_VERSION}"
        )));
    }
    let stored = meta(connection, "embedding_dimension")?;
    if stored != Some(dimension.to_string()) {
        return Err(OpenError::Incompatible(format!(
            "the file holds vectors of width {stored:?}, and {dimension} was configured; \
             changing the embedder is a migration"
        )));
    }
    Ok(())
}
