# CLAUDE.md

Guidance for coding agents working in this repository. It describes the
code, never a machine: anything about one deployment (hosts, GPUs, providers,
paths) lives outside this repo and is pulled in by a gitignored
`CLAUDE.local.md`.

## What this is

A Rust workspace (edition 2024, toolchain pinned in `rust-toolchain.toml`).
Pre-alpha: see the README's Status section for what exists.

## Commands

```bash
cargo build
cargo nextest run --workspace && cargo test --doc --workspace
cargo clippy --workspace --all-targets --all-features
cargo fmt --all
scripts/check_name.sh        # the product name only where allowed
```

## Rules that apply to every change

- **The model is an adversary.** Every model output and every fetched page
  is untrusted input: validate it against a schema, bound its size, never
  let it name a secret, a host for a secret, or a privacy class.
- **Strict schemas at every boundary we own.** Unknown keys are refused;
  every string and array has a maximum. Third-party wire formats are read
  tolerantly, for the fields we use.
- **No panics on model text.** No `unwrap`/`expect` in library code; no
  byte-offset slicing of model output.
- **Failures are visible.** A truncated result says so; a count can be
  drilled to its rows; an empty answer names what does exist.
- **Timestamps are UTC in storage** and local only when shown to a person.
- **The product's name stays out of code** (`scripts/check_name.sh`).
- **Docs move with the code.** Update the README and `docs/` in the same
  change.
- **Commits** follow Conventional Commits; never add attribution to
  an AI assistant or coding agent in commits or pull requests.
