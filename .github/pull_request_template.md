## What and why

<!-- One change. What it does, and why it's needed. -->

## How it's tested

<!-- The tests that show it works. -->

## Checklist

- [ ] Commits follow Conventional Commits (`type(scope): summary`).
- [ ] Docs and README updated where behavior changed.
- [ ] Every new boundary validates against a schema; nothing trusts model output.

### Public API changes only (`plugin-api`, the ports)

The parts of the Rust API Guidelines no tool checks:

- [ ] Conversions use the standard traits (`From`, `TryFrom`, `AsRef`), C-CONV.
- [ ] Constructors are static inherent methods (`new`, `with_…`), C-CTOR.
- [ ] Complex construction uses a builder, C-BUILDER.
- [ ] Newtypes give static distinctions, C-NEWTYPE.
- [ ] Arguments carry meaning through types, not `bool` or `Option`, C-CUSTOM-TYPE.
- [ ] Traits not meant to be implemented outside are sealed, C-SEALED.
- [ ] A breaking change is marked `!` and passes or explains `cargo semver-checks`.
