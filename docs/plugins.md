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
- **The capabilities it needs**, and its record types if it keeps any.

The manifest is the authority. A plugin whose tools differ from it isn't
installed.

## Capabilities

A plugin reaches Excogitate only through capabilities it declared and the
deployment granted, in `plugins.yaml` (schema:
`schemas/v1/plugins_config.schema.json`). Anything not listed there is granted
nothing.

| Capability | What it gives |
|---|---|
| `records` | The plugin's own records in the store, and no one else's. |
| `http` | HTTP through `http_request` itself, in raw mode: the same address guard, redirect checks, credentials and size limits a model's call gets, and the same argument schema. |
| `model` | One-shot model calls through the gateway, under the calling run's privacy class and within what's left of its budget. |

**What the host enforces for `http` and `model`.** A plugin holding either
can bring in text from outside or from a model, so everything its tools
return is labelled untrusted, whatever the plugin says. What its model calls
cost is charged to the run's receipt and budget, even if the plugin reports
less, and whether the tool call worked or failed. The day's spend log sees
every call. A plugin's model calls in one run take turns, so two at once
can't both spend what's left. One gap remains: a tool call stopped by its
time limit reports nothing, so what its model calls cost reaches the
receipt with that plugin's next call in the run, or only the spend log if
there is none. A plugin's model calls are for `plugin:<name>`, so a paid
provider serves them only if its `paid_for` lists that.

No capability lets a plugin call another tool. Excogitate sets each record's
provenance, and the privacy class comes from the run.

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
