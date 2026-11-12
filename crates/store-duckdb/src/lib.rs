//! The default store: an embedded `DuckDB` file, so the runtime needs no database
//! server.
//!
//! The design comes from measuring it (`DuckDB` 2.0):
//!
//! - **One writer.** A single thread makes every write. Job claims go first,
//!   but only so many at a time, or a busy queue starves ingest.
//! - **Exact vector search.** A persisted HNSW index breaks after a crash on
//!   this version, and the scan is fast enough.
//! - **Vectors in their own table,** because the full-text index blocks
//!   altering the table it's built on.
//! - **No primary keys.** Deleting and re-inserting a key in one transaction
//!   trips `DuckDB`'s constraint checks, and with one writer, uniqueness is
//!   checked by a query instead.
//! - **Extensions from a fixed directory,** with automatic install off, so
//!   nothing is fetched at runtime.

mod archive;
mod codec;
mod read;
mod schema_sql;
mod sink;
mod store_impl;
mod writer;

pub use sink::DuckDbSink;
pub use store_impl::{DuckDbConfig, DuckDbStore, OpenError};
