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

| Capability | What it gives | Status |
|---|---|---|
| `records` | The plugin's own records in the store, and no one else's. | Available |
| `http` | HTTP through Excogitate's network rules. | Interface only; a plugin asking for it is refused until the host has it |
| `model` | One-shot model calls, routed and charged by Excogitate. | Interface only, as above |

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

**As its own process.** Call `serve_stdio` from `main`: an MCP server over
stdio, installed with `PluginHost::install_service`. The process gets an empty
environment apart from the variables its spec names, its schemas must match its
manifest, and everything it returns is labelled untrusted. A separate process
can't reach Excogitate's capabilities, so its manifest can't ask for any.
The server doesn't start these yet: `plugins.yaml` can't list one. That
comes with capabilities over the process link, so a plugin can run on the
stock image without a rebuild.
