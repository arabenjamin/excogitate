# The store port

`crates/store` defines what the brain needs from storage. Nothing outside a
store implementation knows which database is underneath.

## What's in it

- **Core kinds:** notes and their chunks, tasks, working memory, jobs, and
  agent and workflow definitions.
- **Records:** one generic kind for plugin data. A plugin declares its record
  types, filterable fields and relations with schemas (`RecordDecls`), and
  reads them with typed filters: equality, ranges and a relation's neighbors.
- **`Store::apply`:** every write is a `UnitOfWork`, a list of `WriteOp`s
  applied whole or not at all. It's data rather than a closure, so a store
  with one writer task can take it as a message.
- **No query strings.** A store plugin that wants to offer raw queries does
  it as its own plugin tool.

Telemetry and trajectories don't go here. They go to the append-only
`EventSink` in `crates/events`.

## Writing a store

1. Implement `NoteReader`, `TaskReader`, `JobQueue`, `DefinitionReader`,
   `RecordReader` and `Store`.
2. Hold a `RecordCatalog` and call it to check records and filters, so what
   "valid" means is the same on every store.
3. Pass the conformance suite, by adding this to the store's tests:

   ```rust
   store_conformance::store_conformance!(MyStore::new(store_conformance::DIMENSION));
   ```

The suite defines what a store is. `crates/store-memory` is the smallest
store that passes it.

## The default store: DuckDB

`crates/store-duckdb` keeps everything in one file, with no database server.
It's built around one writer thread, plain scans for vector search, and a
full-text index maintained by deleting and re-inserting chunks. The reasons
are in the crate's top comment.

**Extensions.** The store loads DuckDB's `fts` extension from a fixed directory
and never downloads anything at runtime. `scripts/fetch_duckdb_extensions.sh`
fetches it for tests, into `.cache/`.

**The library.** The crate links DuckDB's prebuilt shared library, which the
build downloads. A binary needs `libduckdb.so` beside it at runtime.

**Upgrading DuckDB.** A newer DuckDB can write a file an older one can't open.
Before changing the pinned version, call `DuckDbStore::export`, and update
the version in the fetch script (a test checks they match).

**The event sink.** `DuckDbSink` keeps telemetry and trajectories in a file of
its own, so they never compete with the store's writer. A small run index,
written in the same transaction as each run's first and last event, lets runs
be listed by agent and harness. A sink written before the index existed lists
only runs that started after it did.
