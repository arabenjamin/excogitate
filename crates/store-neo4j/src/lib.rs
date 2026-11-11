//! A store on a Neo4j database. It implements the same port as the default
//! store and passes the same conformance suite, so the runtime above it can't
//! tell which one it has. `docs/store.md` describes the graph it writes.
//!
//! Every write in a unit runs in one Neo4j transaction. The job queue and
//! the spend log take a lock node first, so claims and reservations made at
//! the same time, by this process or another, can't both win.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod codec;
mod jobs;
mod read;
mod records;
mod setup;
mod write;

use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use neo4rs::{ConfigBuilder, Graph, Query, Row, Txn};
use store::{Page, PageRequest, RecordCatalog, StoreError};

use codec::from_neo;

/// Where the database is, and the embedding width it holds.
#[derive(Clone)]
pub struct Neo4jConfig {
    /// The Bolt address, such as `bolt://neo4j:7687`.
    pub uri: String,
    /// The user to sign in as.
    pub user: String,
    /// That user's password.
    pub password: String,
    /// The database to use, when it isn't the server's default.
    pub database: Option<String>,
    /// The width of the embeddings it holds. A database keeps the width it
    /// was created with, and refuses a store opened with another.
    pub embedding_dimension: usize,
}

impl std::fmt::Debug for Neo4jConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Neo4jConfig")
            .field("uri", &self.uri)
            .field("user", &self.user)
            .field("password", &"(hidden)")
            .field("database", &self.database)
            .field("embedding_dimension", &self.embedding_dimension)
            .finish()
    }
}

/// A complete store on Neo4j.
pub struct Neo4jStore {
    graph: Graph,
    dimension: usize,
    /// Plugins declare their record types on every start, so the catalog
    /// lives in the process, as it does in every store.
    catalog: RwLock<RecordCatalog>,
}

impl std::fmt::Debug for Neo4jStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Neo4jStore")
            .field("dimension", &self.dimension)
            .finish_non_exhaustive()
    }
}

/// How many times a transaction Neo4j calls transient, such as one that
/// lost a deadlock, is tried again.
const RETRIES: u32 = 4;

/// Runs `$attempt` again while it fails with an error Neo4j says to retry.
macro_rules! retrying {
    ($attempt:expr) => {{
        let mut tries = 0u32;
        loop {
            match $attempt {
                Err(error) if $crate::codec::is_transient(&error) && tries < $crate::RETRIES => {
                    tries += 1;
                    tokio::time::sleep($crate::backoff(tries)).await;
                }
                other => break other,
            }
        }
    }};
}
pub(crate) use retrying;

fn backoff(tries: u32) -> Duration {
    Duration::from_millis(10u64 << tries.min(6))
}

impl Neo4jStore {
    /// Connects, creates the constraints and indexes it needs if they're
    /// missing, and checks the database was made for this store.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if it can't connect or set up.
    /// [`StoreError::Invalid`] if the database holds another embedding
    /// width, or was written by a newer version of this store.
    pub async fn connect(config: &Neo4jConfig) -> Result<Self, StoreError> {
        let mut builder = ConfigBuilder::default()
            .uri(config.uri.as_str())
            .user(config.user.as_str())
            .password(config.password.as_str());
        if let Some(database) = &config.database {
            builder = builder.db(database.as_str());
        }
        let graph = Graph::connect(builder.build().map_err(codec::backend)?)
            .await
            .map_err(from_neo)?;
        let store = Self {
            graph,
            dimension: config.embedding_dimension,
            catalog: RwLock::new(RecordCatalog::new()),
        };
        setup::prepare(&store).await?;
        Ok(store)
    }

    /// The catalog as it is now. A write checks records against this copy,
    /// so a registration in the meantime can't change a unit halfway.
    fn catalog(&self) -> RecordCatalog {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs a query outside a transaction and collects its rows.
    async fn rows(&self, query: Query) -> Result<Vec<Row>, StoreError> {
        let mut stream = self.graph.execute(query).await.map_err(from_neo)?;
        let mut rows = Vec::new();
        while let Some(row) = stream.next().await.map_err(from_neo)? {
            rows.push(row);
        }
        Ok(rows)
    }

    async fn begin(&self) -> Result<Txn, StoreError> {
        self.graph.start_txn().await.map_err(from_neo)
    }
}

/// Runs a query in a transaction and collects its rows.
async fn txn_rows(txn: &mut Txn, query: Query) -> Result<Vec<Row>, StoreError> {
    let mut stream = txn.execute(query).await.map_err(from_neo)?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next(txn.handle()).await.map_err(from_neo)? {
        rows.push(row);
    }
    Ok(rows)
}

/// Commits if the work succeeded, and rolls back if it didn't.
async fn finish<T>(txn: Txn, outcome: Result<T, StoreError>) -> Result<T, StoreError> {
    match outcome {
        Ok(value) => {
            txn.commit().await.map_err(from_neo)?;
            Ok(value)
        }
        Err(error) => {
            // The work's own error is the one worth reporting. A rollback
            // that fails leaves nothing committed either.
            drop(txn.rollback().await);
            Err(error)
        }
    }
}

/// `SKIP` and `LIMIT` for a page, asking for one row more than the page
/// holds, so [`page_of`] can tell whether the list goes on.
fn page_clause(request: PageRequest) -> String {
    format!(
        "SKIP {} LIMIT {}",
        request.offset(),
        u64::from(request.limit()) + 1
    )
}

/// Turns rows fetched with [`page_clause`] into a page.
fn page_of<T>(
    rows: &[Row],
    request: PageRequest,
    decode: impl Fn(&Row) -> Result<T, StoreError>,
) -> Result<Page<T>, StoreError> {
    let limit = request.limit() as usize;
    let has_more = rows.len() > limit;
    let items = rows
        .iter()
        .take(limit)
        .map(decode)
        .collect::<Result<Vec<T>, StoreError>>()?;
    Ok(Page { items, has_more })
}

#[cfg(test)]
mod tests;
