# Writing a plugin

Excogitate has opinions on how a plugin is built and none on what it does.
Copy `plugins/example` to start. Its tests run it both ways, through the real
host and dispatcher.

## The manifest

`plugin.yaml` (schema: `schemas/v1/plugin_manifest.schema.json`) declares:

- **The name.** It prefixes everything: tools are `name_tool`, and record types
  and relations are `name.type`. Names the core keeps for itself are refused:
  `harness`, `http`, `memory`, `delegate`, `chat`, `runtime`, `core`,
  `plugin`, `plugins`, `store`, `system` and `tool`.
- **Every tool**, with strict input and output schemas and its `effect` (`read`
  or `write`). Every string and array needs a maximum, and unknown keys must be
  refused.
- **The capabilities it needs**, its record types if it keeps any, and the
  sources it offers memory, if any.

The manifest is the authority. A plugin whose tools differ from it isn't
installed.

## Capabilities

A plugin reaches Excogitate only through capabilities it declared and the
deployment granted, in `plugins.yaml` (schema:
`schemas/v1/plugins_config.schema.json`). Anything not listed there is granted
nothing.

| Capability | What it gives |
|---|---|
| `records` | The plugin's own records in the store, and no one else's. `put_many` and `link_many` write up to 500 at once as one unit: all of a batch, or none of it if any part is refused. |
| `http` | HTTP through `http_request` itself, in raw mode: the same address guard, redirect checks, credentials and size limits a model's call gets, and the same argument schema. |
| `model` | One-shot model calls through the gateway, under the calling run's privacy class and within what's left of its budget. |
| `memory` | Notes in the runtime's long-term memory, so its search finds what the plugin brought in. A note is kept under the calling run's privacy class, marked untrusted, and stored by `plugin:<name>`; its embedding is paid from what the run has left. The limits are the `memory` tool's: text up to 100,000 characters, a title up to 256 (the first line when empty), 16 tags. |
| `jobs` | Starting workflow runs, each its own job, as `POST /api/v1/workflows/{name}/runs` starts them: the workflow must exist and the inputs fit. A run is held to its workflow's own budget, not to what the calling run has left, and is never retried. The plugin reads back only the jobs it started. |
| `sources` | Offering memory the sources its manifest declares, to search beside its own notes. Nothing is handed to the plugin; see [Sources](#sources). |

**Loading a lot.** A plugin bringing in data in bulk, such as rows from a
lake, writes them in batches: `put_many` returns the ids in order, then
`link_many` links them. Every name in a batch is checked against the
plugin's prefix, and every record against its type's schema, before
anything is written. A batch must also fit in one message to the runtime,
8 MiB for a plugin run as a process, so very large records take smaller
batches. Records calls don't count against a run's tool calls; `http` and
`model` calls do.

**Starting work.** `run_workflow` queues a run and returns its job; with an
idempotency key, the same key starts the workflow once for that plugin,
which is how a feed fans out to one run per new item without repeats. A key
is remembered as long as its job is kept: once `retention.job_days` prunes
the finished job (30 days by default), the same key starts the workflow
again, so a feed that can list an item for longer keeps its own record of
what it has seen. A
plugin may have at most 100 jobs waiting or running at once; past that a
start is refused, so a workflow that calls the plugin again can't multiply
without end. Only workflow runs can be started, never Harness² or any other
kind of job. `job` reads a job back, with its result once it's finished; a
workflow run is private, so a run that isn't gets its phase without the
result. Each start and each read counts as one of the run's tool calls.

**In a Harness² probe,** a plugin's tools may run only if they're on the
probe's read list, and even then a write through `records` or `memory`
passes its checks and isn't made, and a run passes its checks and isn't
started, as a probe's other writes aren't. An `http` request other than
`GET` or `HEAD` is refused in a probe.

**What the host enforces for `http`, `model`, `memory` and `jobs`.** A plugin
holding any of them can bring in text from outside or from a model, so everything its
tools return is labelled untrusted, whatever the plugin says. What its model
calls and its notes' embedding cost is charged to the run's receipt and
budget, even if the plugin reports less, and whether the tool call worked or
failed. Each call counts as one of the run's tool calls. The day's spend log
sees every call. A plugin's model calls and note stores in one run take
turns, so two at once can't both spend what's left. A plugin's notes are
embedded for the purpose `memory`, as every note is, so a paid embedding
provider serves them only if its `paid_for` lists `memory`. One gap remains: a tool call stopped by its
time limit reports nothing, so what its model calls cost reaches the
receipt with that plugin's next call in the run, or only the spend log if
there is none. A plugin's model calls are for `plugin:<name>`, so a paid
provider serves them only if its `paid_for` lists that.

No capability lets a plugin call another tool. Excogitate sets each record's
provenance, and the privacy class comes from the run.

## Sources

A source is something memory reads that lives outside the store, such as a
data lake. The plugin keeps it and its own ingestion; memory searches it
beside its notes and fuses the hits by rank, one more list in the same
reciprocal rank fusion. A source only reads, and nothing writes to it.

```yaml
capabilities: [sources]
sources:
  - name: glossary         # memory calls it example.glossary
    description: A few words the runtime uses, and what they mean.
    public: false          # the default: every item it returns is private
    words: true            # it can be searched by words
    vectors:               # left out if it holds no vectors
      model: an-embedder   # as the model catalog names it, or its wire model
      dimension: 1024
```

The plugin builds each one in `Plugin::sources`, implementing `Source`:
`search(query, context)` returns hits best first, and `get(id, context)`
returns one item whole, or none, as a `SourceGot`. A plugin run as a process
answers `sources/search` and `sources/get` over the same connection as its
tools, and `serve_plugin_stdio` does that for it. Their parameters and
answers are published as `source_search`, `source_get`, `source_found` and
`source_got` ([schemas.md](schemas.md)); a source that can't answer replies
with error code -32010.

- **Privacy is checked twice.** The source gets the run's class in its
  `ToolContext`, and memory then drops any item the run may not see. An
  item's own class counts only from a source declared `public`; from any
  other, every item is private. Items are always untrusted.
- **Vectors are optional.** Memory sends the query's vector only when the
  model that made it is the one the source names, at its width. Otherwise a
  source is searched by words, and a source with only vectors from a model
  the deployment doesn't have stops the server at startup. A manifest
  declaring neither words nor vectors is refused.
- **Bounds.** A search asks for at most 50 hits and keeps what it asked
  for. Ids are up to 256 characters, titles are cut to 256, a hit's text to
  8,000 and an item read whole to 100,000, marked `truncated`.
- **Time and money.** Each search of a source has its own limit inside the
  memory call's: `source_time_limit_secs` for that plugin in `plugins.yaml`,
  5 by default. A source that fails or runs out of time is left out and
  named in `sources_skipped`, and the rest of the search goes on. Sources
  are asked one after another, each with the money, tool calls and time the
  search has left after the ones before it. What a source reports it spent,
  and what its metered capability calls cost, go on the memory call's cost,
  a call cut off for time included, and each capability call counts as one
  of the run's tool calls.
- **A source only reads.** Its `ToolContext` has `reading` set, and while
  it's set a capability call that writes is refused: `records` and `memory`
  writes, starting a job, and any `http` request but `GET` or `HEAD`.
- **Agents choose.** An agent's `memory.sources` names the sources its
  memory reads; left out, it reads them all. See
  [workflows.md](workflows.md#agents).
- **Not in the hook:** SQL, table schemas or any query language. A query is
  text and maybe a vector, and an item is text.

## The contract

A plugin depends on `plugin-api` and nothing else from Excogitate: the store
and run types it needs (`Record`, `RecordQuery`, `RunId`, `Trust` and the
rest) are re-exported there.

- **Build values with their constructors,** such as `ToolOutput::new`,
  `ToolFailure::new`, `HttpRequest::get`, `RecordQuery::new` and
  `Capabilities::default().with_records(...)`. The types are
  `#[non_exhaustive]`, so a later release can add a field or a variant
  without breaking a plugin.
- **Pass the call's `ToolContext` to every capability call.** It says which
  run, and in a process which tool call, the request is for.
- **What's hidden isn't the contract.** `plugin_api::wire`,
  `plugin_api::rpc`, `McpHandler`, `serve` and `read_line` are the host's
  side. They're public only so the runtime can use them, and they can
  change in any release.

From v0.4.0, a release that breaks the contract says so in its version and
its changelog. CI holds it to that: `cargo semver-checks` compares
`plugin-api` with the last release, and the schema check compares the
process protocol's schemas (see `docs/schemas.md`).

## Two ways to run

**Compiled into your own binary.** Plugins live in your own repo, not this
one; this repo keeps only the example. Your repo has a small binary crate
that depends on Excogitate's crates at a release tag and lists your plugins:

```toml
# Cargo.toml
[package]
name = "my-instance"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "excogitate"   # the binary's name prefixes every setting
path = "src/main.rs"

[dependencies]
runtime = { git = "https://github.com/arabenjamin/excogitate", tag = "vX.Y.Z" }
plugin-api = { git = "https://github.com/arabenjamin/excogitate", tag = "vX.Y.Z" }
my-plugin = { path = "plugins/my-plugin" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
// src/main.rs
use std::process::ExitCode;
use std::sync::Arc;

#[tokio::main]
async fn main() -> ExitCode {
    let program = runtime::Program::new(env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
        .with_plugin(Arc::new(my_plugin::MyPlugin));
    runtime::main(program).await
}
```

That's the whole binary: `crates/app` here is the same thing with the
example behind a feature. Add `features = ["neo4j"]` to `runtime` for the
Neo4j store. To build an image, copy this repo's `Dockerfile`: the binary
needs `libduckdb.so` and DuckDB's extensions beside it.

What each plugin may use is granted in `plugins.yaml`, named by
`EXCOGITATE_PLUGINS`. Without that file nothing is granted, and a plugin
compiled in that asks for a capability stops the start with a message
saying which. A plugin's tools are offered to chats, may be named by agents
and workflows, and are listed over MCP, all through dispatch, like the
bundled ones.

**As its own process, on the stock image.** The same plugin crate, with no
rebuild of Excogitate: a binary whose `main` is one line,

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    plugin_api::serve_plugin_stdio(&my_plugin::MyPlugin).await
}
```

and an entry in `plugins.yaml` that says how to start it:

```yaml
plugins:
  - name: my_plugin
    capabilities: [records, http]
    process:
      command: /config/plugins/my-plugin       # mount it read-only into the container
      manifest: /config/plugins/my-plugin.yaml # the same plugin.yaml it compiles with
      env: [MY_PLUGIN_TOKEN]                   # all it sees of the environment
      timeout_secs: 30
```

It speaks MCP over stdio, and gets the capabilities its manifest declares as
proxies: each call goes back to Excogitate over the same connection. The host
holds it to the same rules as a compiled-in plugin, and some more:

- **Only during a call.** Each tool call carries a token of its own, and a
  capability call must bring back the token of a call the host is serving
  on that connection right now. Between calls it's refused, and when a call
  ends, whatever it still has running is stopped.
- **Bounded.** A process can have 8 capability calls running at once; more
  are refused, not queued. Each request must meet
  `schemas/v1/capability_request.schema.json`, limits included, before the
  host does anything with it.
- **The host's record, not the process's.** The run's privacy class and what's
  left of its budget come from the host's own record of the call. The process
  can't claim more.
- **Granted or refused before it starts.** A plugin asking for a capability
  `plugins.yaml` doesn't grant is never started. Its manifest's name must be
  the entry's name, and the tools it serves must match the manifest exactly.
- **Everything it returns is untrusted,** and what its model calls cost is
  charged to the run.
- **An empty environment** but the variables `env` names, and the call fails if
  it takes longer than `timeout_secs`.

`plugins/example` runs both ways from one `plugin.yaml`; its tests run it as a
process, records and all.
