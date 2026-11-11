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

A plugin reaches the brain only through capabilities it declared and the
deployment granted, in `plugins.yaml` (schema:
`schemas/v1/plugins_config.schema.json`). Anything not listed there is granted
nothing.

| Capability | What it gives | Status |
|---|---|---|
| `records` | The plugin's own records in the store, and no one else's. | Available |
| `http` | HTTP through the brain's network rules. | Interface only; a plugin asking for it is refused until the host has it |
| `model` | One-shot model calls, routed and charged by the brain. | Interface only, as above |

No capability lets a plugin call another tool. The brain sets each record's
provenance, and the privacy class comes from the run.

## Two ways to run

**Compiled in.** Implement `Plugin`, and add the crate to `crates/app` behind a
cargo feature. The default build has no plugins.

**As its own process.** Call `serve_stdio` from `main`: an MCP server over
stdio, installed with `PluginHost::install_service`. The process gets an empty
environment apart from the variables its spec names, its schemas must match its
manifest, and everything it returns is labelled untrusted. A separate process
can't reach the brain's capabilities, so its manifest can't ask for any.
