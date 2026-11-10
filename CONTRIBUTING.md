# Contributing

Thanks for helping. This file covers how to build, test and send a change.
The rules a change has to meet are the ones CI checks, so run the checks
before you push.

## Build and test

You need Rust 1.99.1, which `rust-toolchain.toml` pins (rustup picks it up),
and Docker for the secret scan.

```bash
cargo build
cargo nextest run --workspace     # or: cargo test --workspace
cargo test --doc --workspace      # nextest skips doctests
cargo clippy --workspace --all-targets --all-features
cargo fmt --all
```

Turn on the hooks once per clone:

```bash
git config core.hooksPath .githooks
```

Schemas in `schemas/` are generated from the Rust types and a test fails
when they drift. After changing a type behind one, run
`cargo run -p schema-gen -- --write` and commit the result.

`pre-commit` refuses any file over 1 MiB, then runs a secret scan of what's
staged, the format check, the name check and the unit tests. `commit-msg` checks the message format.

## Branches and commits

- **Trunk-based.** Make a short branch off `main` and open a pull request
  into `main`. A release is a tag on `main`.
- **Conventional Commits.** Write `type(scope): summary`, where type is one
  of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`,
  `ci`, `chore` or `revert`, and `!` marks a breaking change. CI checks
  every commit in a pull request, and `CHANGELOG.md` is generated from
  them by git-cliff.
- **One change per pull request**, with the tests that show it works.

## What CI enforces

- **Style:** `rustfmt`, which is the Rust Style Guide. There is no
  `rustfmt.toml`, because the edition sets the style.
- **Lints:** set in `[workspace.lints]` in `Cargo.toml`. They are deny from
  the start: missing docs, missing `Debug` implementations, non-standard
  naming, `unsafe`, `unwrap`/`expect` outside tests, broken doc links, and
  clippy's pedantic group (each `allow` says why).
- **Dependencies:** `cargo deny` checks licenses, bans and sources on each
  push. The advisory scan runs daily on its own, so a newly published
  advisory doesn't block an unrelated push.
- **Spelling and TOML format:** `typos` and `taplo`.
- **The name:** the product's name appears only in the places
  `scripts/name-allowlist` lists (the binary, the image, the env prefix, the
  docs). `scripts/check_name.sh` fails anywhere else, which keeps a rename
  a small scripted change. Inside code, say "the brain" or use `crate::`.

## Reviewing a public API

Not everything in the Rust API Guidelines can be checked by a tool. For a
change to a public API (`plugin-api`, the ports), the pull request template
asks for the rest: conversions (C-CONV), constructors (C-CTOR), builders
(C-BUILDER), newtypes (C-NEWTYPE), custom types over bools (C-CUSTOM-TYPE),
and sealed traits (C-SEALED).

## Licensing of contributions

Inbound = outbound. Unless you explicitly state otherwise, any contribution
you intentionally submit for inclusion in the work, as defined in the
Apache-2.0 license, is dual licensed under MIT OR Apache-2.0, without any
additional terms or conditions.

## CI timings

The target is a warm push in under 5 minutes, with the quick checks failing
in under a minute. Measured 2026-11-10: a push took 82 seconds, and the quick
checks 18. That's before the bundled database build arrives in Phase 2a, so
measure again then.
