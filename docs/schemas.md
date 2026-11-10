# Schemas

Every boundary is checked against a JSON Schema (2020-12). The schemas in
`schemas/v1/` are generated from the Rust types by `crates/schema-gen`.

## The rules

The check in `schema::check_strict` runs on every generated schema, and on
every schema a plugin declares when it registers:

- an object sets `additionalProperties: false`;
- a string has a `maxLength`, unless it's an enum, a const, a `uuid` or a
  `date-time`;
- an array has a `maxItems`;
- a place that really is open declares it with `x-extension`, whose value
  says why.

## Changing one

1. Change the Rust type.
2. Run `cargo run -p schema-gen -- --write`.
3. Commit the type and the file together.

A test fails when a committed file differs from its type, when a type's
schema isn't strict, or when a file has no type behind it. To publish a new
schema, add its type to `generated` in `crates/schema-gen/src/main.rs`.

## Validating a value

`schema::Validator` checks untrusted JSON against a schema, then converts it.
A refusal lists what failed, in a bounded form that can go back to a model.
