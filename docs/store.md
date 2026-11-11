# The store port

`crates/store` defines what Excogitate needs from storage. Nothing outside a
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
- **No query strings.** A plugin that wants to offer raw queries does it as
  its own tool.

Telemetry and trajectories don't go here. They go to the append-only
`EventSink` in `crates/events`.

## Writing a store

1. Implement `NoteReader`, `TaskReader`, `JobQueue`, `DefinitionReader`,
   `RecordReader`, `SpendLog` and `Store`.
2. Hold a `RecordCatalog` and call it to check records and filters, so what
   "valid" means is the same on every store.
3. Pass the conformance suite, by adding this to the store's tests:

   ```rust
   store_conformance::store_conformance!(MyStore::new(store_conformance::DIMENSION));
   ```

   A store whose cases must share one database writes `held` before an
   expression giving `(store, guard)`, and the guard is kept until the case
   ends. The Neo4j store does this.

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

## The Neo4j store

`crates/store-neo4j` keeps everything in a Neo4j 5 database. It's compiled
in with the `neo4j` feature (`app/neo4j` for the image, through the
`FEATURES` build argument) and chosen with `EXCOGITATE_STORE=neo4j`; the
settings are in [env](env.md). DuckDB stays the default. Unlike the DuckDB
file, the graph can be opened by several processes at once. The event log
stays in a DuckDB file in the data directory either way.

**The graph.** Each core kind is a label: `Note`, `Chunk` (linked from its
note by `HAS_CHUNK`), `Task`, `Working`, `Job`, `Definition` and `Spend`.
Plugin records are `Record` nodes keyed by `type` and `id`, and a relation
between them is a `LINK` relationship with a `relation` property. Times are
native `DATETIME`s in UTC. JSON values (payloads, results, record data,
definition bodies) are stored as JSON text, since a property can't nest a
map. A record's filterable fields are copied into typed properties named
`field_<name>`, so filters run in the database. A day's spend is decimal
text, because it may be any `u64` and Neo4j integers are signed 64-bit.

**Setup.** On connecting, the store creates its constraints and indexes if
they're missing and waits for them to come online. A `StoreMeta` node
records the schema version and the embedding width. A database written by a
newer store, or holding another width, is refused.

**Search.** Full-text search uses a Lucene index over each chunk's words,
normalized the same way as in the other stores (lowercase letters and
digits), with no stop words. Vector search is an exact scan with
`vector.similarity.cosine`, as the default store's is. A zero vector scores
0.

**Concurrency.** Every unit of work is one transaction. The job queue takes
a lock node before any claim, completion or failure, and a spend
reservation locks its day, so claims or reservations made at once by any
number of processes can't both win. A transaction Neo4j calls transient,
such as a deadlock, is retried a few times.

**Its tests.** Neo4j Community has one database, so the conformance cases
take turns with it: each empties it and runs. They need a scratch database
named by `NEO4J_TEST_URI` and `NEO4J_TEST_PASSWORD`. The first run marks it
as scratch, and a database that holds data without the mark is refused,
never emptied. Without `NEO4J_TEST_URI` the cases skip, unless
`NEO4J_TEST_REQUIRED` is set, as it is in CI. Under nextest they run one at
a time (`.config/nextest.toml`).
