# Schemas

Every boundary is checked against a JSON Schema (2020-12). The schemas in
`schemas/v1/` are generated from the Rust types by `crates/schema-gen`.

## The rules

The check in `schema::check_strict` runs on every generated schema, and on
every schema a plugin declares when it registers:

- an object sets `additionalProperties: false`;
- a string has a `maxLength`, unless it's an enum, a const, a `uuid`, a
  `date-time` or a `date`;
- an array has a `maxItems`;
- a place that really is open declares it with `x-extension`, whose value
  says why.

## Changing one

1. Change the Rust type.
2. Run `cargo run -p schema-gen -- --write`.
3. Commit the type and the file together.

A test fails when a committed file differs from its type, when a type's
schema isn't strict, or when a file has no type behind it. To publish a new
schema, add its type to `fixed` in `crates/schema-gen/src/main.rs`, and
who reads it to `role`.

## Compatibility

CI checks every schema against the one the last release published (the
newest `v*` tag). What breaks depends on who reads the schema, which
`role` in `crates/schema-gen` names for each one:

- **What Excogitate accepts** (configuration, definitions, requests, a
  tool's arguments, a plugin's capability requests): the new schema must
  accept everything the old one did. Removing a property, a value or a
  type, requiring a property, or tightening a bound breaks it. Adding an
  optional property or loosening a bound doesn't.
- **What Excogitate produces** (responses, receipts, trajectories, a tool's
  result, what a plugin is sent): everything it may now produce must still
  pass the old schema. Adding a value to an enum or a kind of step, making a
  property optional, or loosening a bound breaks it. Adding a property
  doesn't: a reader built against the old schema ignores it.

A removed schema is a break, and a changed `pattern`, `format` or condition
is treated as one. A break passes only when the workspace version is past
the tag's (the minor before 1.0, the major after), so it's declared. To run
the check yourself:

```bash
mkdir -p /tmp/baseline && git archive v0.4.0 schemas/v1 | tar -x -C /tmp/baseline
cargo run -p schema-gen -- --against /tmp/baseline/schemas/v1 --since v0.4.0
```

## What's published

- **Configuration and definitions:** `config`, `models_config`,
  `plugins_config`, `http_config`, `plugin_manifest`, `record_decls`,
  `agent`, `workflow`, `schedule`.
- **The REST API's bodies,** such as `run_request`, `receipt` and
  `api_error`.
- **Each bundled tool's arguments and result:** `tool_<name>_input` and
  `tool_<name>_output`, for `http_request`, `memory`, `delegate` and `chat`.
- **An export's files:** `export_manifest`, and `export_<kind>` for each
  file's lines. An import accepts what an older export wrote, so they're
  held to the rules for what Excogitate accepts. See
  [export and import](store.md#export-and-import).
- **The plugin process protocol:** `tool_call_context` is what a tool call
  carries in `_meta.context`, `capability_request` is the parameters of a
  `capabilities/call` request, and `capability_record`, `capability_page`,
  `capability_http_response` and `capability_model_reply` are what comes
  back. The host checks every capability request against
  `capability_request`, bounds included, before it does anything.

## Validating a value

`schema::Validator` checks untrusted JSON against a schema, then converts it.
A refusal lists what failed, in a bounded form that can go back to a model.
