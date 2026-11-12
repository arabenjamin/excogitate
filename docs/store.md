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

Stores are crates in the runtime, chosen by a build feature, not plugins:
a deployment's own store is a change to the runtime. So the store port and
the event sink (`crates/events`) are internal before 1.0, and a release may
change them, saying so in its changelog. The plugin contract (`plugin-api`)
and the published schemas are what's held stable.

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
Before changing the pinned version, make an [export](#export-and-import) (or
call `DuckDbStore::export` for DuckDB's own), and update the version in the
fetch script (a test checks they match).

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

## Export and import

`export <dir>` writes everything the store holds into a new directory, and
`import <dir>` loads it into an empty store. That's how to back up, move
between DuckDB and Neo4j, or leave.

```bash
docker compose stop server
docker compose run --rm -v ./backup:/export session export /export/2027-01-02
docker compose start server
```

The container runs as uid 1000, which must be able to write `./backup`.

- **One file per kind,** one JSON object per line: `notes.jsonl`,
  `chunks.jsonl` (with embeddings, each note's after it), `tasks.jsonl`,
  `working.jsonl` (what hasn't expired), `jobs.jsonl` (finished ones too,
  with their results), `definitions.jsonl`, `records.jsonl`, `links.jsonl`
  and `spend.jsonl`. Each line's schema is published as
  `schemas/v1/export_<kind>.schema.json`, so DuckDB or any other tool can
  read the files as they are.
- **`manifest.json` is written last:** the format's version, the runtime
  that made it, the store, the embedding width and each file's count. An
  export that stopped part way has none, and can't be imported.
- **Not in it:** the event log (telemetry and trajectories, in its own file
  and pruned), and the record catalog, which plugins declare when they
  start.
- **Stop the server first.** Both commands open the store themselves. DuckDB
  refuses a second process, which keeps an export a snapshot. An export
  from Neo4j beside a running server is read a page at a time, and isn't
  one.
- **An import checks everything before it writes anything:** the format's
  version, every line against its schema, each file against the manifest's
  count, chunks in their notes' order, the embedding width against this
  store's, an empty store (no notes, tasks, jobs or records; definitions may
  be there, since the server loads its files), and that this build's
  plugins declare every record type and relation the export holds.
- **It writes in batches.** If a write fails part way, it says so, and the
  store holds part of the export: start again from an empty one.
- **Jobs come back exactly as they were,** so finished work isn't queued
  again and retention still prunes it. Notes keep their ids and times, and
  their versions start again at 1.

**For a store's author:** `Archive` lists every note, working entry, record
(whatever its type, without the catalog), link and spend row, a page at a
time in a stable order, and `WriteOp::RestoreJob` adds a job as it was.
The conformance suite checks both.
