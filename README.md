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
agents](docs/harness.md). CI runs the quickstart below with a scripted model,
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
- **It needs no database server.** Memory, retrieval and the job queue live
  in an embedded DuckDB file. Other stores plug in behind the same port.
- **It takes no side on models.** It works with any endpoint that speaks the
  OpenAI wire format, local or cloud.
- **It ships two tools,** `http_request` and `memory`. Everything else comes
  from plugins. Excogitate has opinions on how a plugin is built, and none
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
cp brain.example.yaml brain.yaml
echo "EXCOGITATE_OWNER_KEY=$(openssl rand -hex 32)" >> .env
docker compose up -d --build
```

Ask it something:

```sh
source .env
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $EXCOGITATE_OWNER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"brain","messages":[{"role":"user","content":"Read https://example.com and remember what it says."}]}'
```

The reply has the answer and a receipt of what actually ran. Memory lives in
the `brain-data` volume, so it's still there after a restart.

Any OpenAI client works with the base URL `http://127.0.0.1:8080/v1`, and an MCP
client with `http://127.0.0.1:8080/mcp`. Both use the key as a bearer token.
See [the API](docs/api.md). An MCP client that only speaks stdio can
[relay through `connect`](docs/mcp.md#beside-the-server).

## Configuration

Settings live in `models.yaml` (see [models](docs/models.md)) and `brain.yaml`,
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
[plugins](docs/plugins.md) and [schemas](docs/schemas.md).

## Writing a plugin

A plugin has a manifest with a schema, a declared schema for every tool's
input and output, and narrow capabilities granted by name. It can be compiled
in behind a cargo feature or run as its own process. Start by copying
[`plugins/example`](plugins/example), and see [the guide](docs/plugins.md).

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
