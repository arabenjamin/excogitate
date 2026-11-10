# Excogitate

A harness that gets better results from small language models, and improves
itself from evidence.

[![CI](https://github.com/arabenjamin/excogitate/actions/workflows/ci.yml/badge.svg)](https://github.com/arabenjamin/excogitate/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.99.1](https://img.shields.io/badge/rust-1.99.1-orange.svg)](rust-toolchain.toml)

## Status

**Pre-alpha: nothing here runs yet.** The interfaces are being built first
(Phase 1: the schemas, the store and model ports, dispatch and the plugin
contract are in place, and nothing uses them end to end yet). The
first usable release, Phase 2a, is a brain that can look something up on the
web and remember it.
Until then, expect everything to change, and don't depend on any interface.

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
  runs, and proposes only the changes the evidence supports. A person
  adopts them.
- **It needs no database server.** Memory, retrieval and the job queue live
  in an embedded DuckDB file. Other stores plug in behind the same port.
- **It takes no side on models.** It works with any endpoint that speaks the
  OpenAI wire format, local or cloud.
- **It ships two tools,** `http_request` and `memory`. Everything else comes
  from plugins. Excogitate has opinions on how a plugin is built, and none
  on what it does.

## Requirements

Planned for the first release, to be measured before it ships:

- **Docker** with Compose, on Linux (amd64 or arm64) or macOS.
- **A model endpoint** that speaks the OpenAI wire format, for chat and,
  for vector search, embeddings. Without embeddings, search falls back to
  keywords and says so.
- **About 1.5 GB of RAM** and a few hundred MB of disk for a store of tens
  of thousands of notes.

## Quickstart

Not yet. The quickstart arrives with Phase 2a, and CI will run it, so this
section can't drift from what works.

## Configuration

Every variable starts with `EXCOGITATE_`. The reference will be in
`docs/env.md`, along with `.env.example`.

## Architecture

The plan has four layers: a store port (DuckDB by default), a model port
(any OpenAI-wire endpoint), a dispatcher that validates every call, and the
agents, workflows and Harness² on top. `docs/` describes each one as it is
built: so far, [the store port](docs/store.md), [the model
port](docs/models.md), [dispatch](docs/dispatch.md), [plugins](docs/plugins.md) and
[schemas](docs/schemas.md).

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
