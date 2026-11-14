# Excogitate

A harness that gets better results from small language models, and improves
itself from evidence.

[![CI](https://github.com/arabenjamin/excogitate/actions/workflows/ci.yml/badge.svg)](https://github.com/arabenjamin/excogitate/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.99.1](https://img.shields.io/badge/rust-1.99.1-orange.svg)](rust-toolchain.toml)

## Status

**Pre-alpha.** It serves [MCP, an OpenAI-compatible endpoint and a REST
API](docs/api.md): it can chat, read a URL, remember what it read and run
[workflows](docs/workflows.md) on a schedule, and [improve its own
agents](docs/harness.md). It can also offer a [remote MCP
server's tools](docs/mcp.md#remote-servers) as its own, and serves [Prometheus
metrics](docs/metrics.md). CI runs the quickstart below with a scripted model,
Harness² included. [Recall on a small local model](docs/mcp.md#recall) has been measured
only on a small set of short notes.
Expect everything to change, and don't depend on any interface.

## What it is, and why

Small models, run locally or cheaply in the cloud, are good enough for a lot
of work when they're given the right harness: tight prompts, strict
schemas, budgets, and memory. Excogitate is that harness.

- **It treats the model as an adversary.** Every model output is untrusted
  input. Tool calls are checked against their schemas, secrets never pass
  through the model, and every answer carries a receipt of what actually
  ran.
- **It improves its own agents from evidence (Harness²).** It records each
  run, tries candidate changes with side effects stubbed, compares the
  runs, and adopts only the changes the evidence supports. It tells you of
  each one, and you can revert any of them.
- **An agent's rules are kept in code, not hoped for.** A guardrail refuses
  a call the agent mustn't make, caps a tool, or sends an answer back until
  a required tool has run. A skill adds instructions only to the tasks that
  match it, so a small model's prompt stays small.
- **It needs no database server.** Memory, retrieval and the job queue live
  in an embedded DuckDB file. Other stores plug in behind the same port,
  and a [Neo4j store](docs/store.md#the-neo4j-store) comes with it, behind a
  build feature.
- **It takes no side on models.** It works with any endpoint that speaks the
  OpenAI wire format, local or cloud.
- **It ships three tools,** `http_request`, `memory` and `job_history`.
  Everything else comes from plugins. Excogitate has opinions on how a plugin is built, and none
  on what it does.

## Requirements

- **Docker** with Compose. CI runs it on Linux amd64. The rest (arm64, macOS)
  should work, and hasn't been checked. The image is about 220 MB.
- **A model endpoint** that speaks the OpenAI wire format and supports tool
  calls. For vector search, also embeddings; without them, search falls back
  to keywords and says so.
- **`openssl` and `curl`**, for the quickstart.
- **About 1.5 GB of RAM** for a store of tens of thousands of notes.

## Quickstart

```sh
git clone https://github.com/arabenjamin/excogitate.git
cd excogitate
cp .env.example .env
cp models.example.yaml models.yaml   # then point it at your model server
cp config.example.yaml config.yaml
echo "EXCOGITATE_OWNER_KEY=$(openssl rand -hex 32)" >> .env
docker compose up -d --build
```

Ask it something:

```sh
source .env
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $EXCOGITATE_OWNER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"excogitate","messages":[{"role":"user","content":"Read https://example.com and remember what it says."}]}'
```

The reply has the answer and a receipt of what actually ran. Memory lives in
the `data` volume, so it's still there after a restart.

Any OpenAI client works with the base URL `http://127.0.0.1:8080/v1`, and an MCP
client with `http://127.0.0.1:8080/mcp`. Both use the key as a bearer token.
See [the API](docs/api.md). An MCP client that only speaks stdio can
[relay through `connect`](docs/mcp.md#beside-the-server).

To back up, or to move to another store, stop the server and run
`export <dir>`; `import <dir>` loads it into an empty store. See
[export and import](docs/store.md#export-and-import).

## Configuration

Settings live in `models.yaml` (see [models](docs/models.md)) and `config.yaml`,
both checked against their schemas at startup. Keys and paths are environment
variables; [`docs/env.md`](docs/env.md) has the list.

## Architecture

The plan has four layers: a store port (DuckDB by default), a model port
(any OpenAI-wire endpoint), a dispatcher that validates every call, and the
agents, workflows and Harness² on top. `docs/` describes each one as it is
built: so far, [the store port](docs/store.md), [the model
port](docs/models.md), [memory and retrieval](docs/memory.md), [dispatch](docs/dispatch.md),
[the bundled tools](docs/tools.md), [MCP](docs/mcp.md), [the API](docs/api.md), [work](docs/work.md),
[workflows](docs/workflows.md), [Harness²](docs/harness.md),
[plugins](docs/plugins.md), [schemas](docs/schemas.md) and [metrics](docs/metrics.md).

## Writing a plugin

A plugin has a manifest with a schema, a declared schema for every tool's
input and output, and narrow capabilities granted by name. It can also offer
memory a source to read beside its own notes, such as a data lake. Plugins live in
your own repo: run one as its own process on the stock image, listed in
`plugins.yaml`, or compile them into a small binary of your own. Either way,
nothing in Excogitate changes. Start one with `scripts/plugin_template.sh new`, which
stamps a plugin that already meets the contract and passes its tests; see
[the guide](docs/plugins.md), the worked [`plugins/example`](plugins/example),
and [`examples/`](examples), a daily news brief built end to end from plugins.

## Getting help, contributing, security

- **Questions and bugs:** [GitHub issues](https://github.com/arabenjamin/excogitate/issues).
- **Contributing:** read [CONTRIBUTING.md](CONTRIBUTING.md) and the
  [Code of Conduct](CODE_OF_CONDUCT.md).
- **Security:** don't open a public issue. See [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
