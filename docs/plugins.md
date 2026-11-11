# Writing a plugin

Excogitate has opinions on how a plugin is built and none on what it does.
Copy `plugins/example` to start. Its tests run it both ways, through the real
host and dispatcher.

## The manifest

`plugin.yaml` (schema: `schemas/v1/plugin_manifest.schema.json`) declares:

- **The name.** It prefixes everything: tools are `name_tool`, and record types
  and relations are `name.type`.
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
less. The day's spend log sees every call. One gap: when the tool call
itself fails, what its model calls cost is in the spend log but not on the
run's receipt. A plugin's model calls are for `plugin:<name>`, so a paid
provider serves them only if its `paid_for` lists that.

No capability lets a plugin call another tool. Excogitate sets each record's
provenance, and the privacy class comes from the run.

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
    runtime::main(runtime::Program {
        name: env!("CARGO_BIN_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        plugins: vec![Arc::new(my_plugin::MyPlugin)],
    })
    .await
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

- **Only during a call.** Each tool call carries its run, and a capability
  call must name a run the host is serving on that connection right now.
  Between calls, or for another run, it's refused.
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
