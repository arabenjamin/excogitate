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
