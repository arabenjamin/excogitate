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
schema, add its type to `fixed` in `crates/schema-gen/src/main.rs`.

## What's published

- **Configuration and definitions:** `config`, `models_config`,
  `plugins_config`, `http_config`, `plugin_manifest`, `record_decls`,
  `agent`, `workflow`, `schedule`.
- **The REST API's bodies,** such as `run_request`, `receipt` and
  `api_error`.
- **Each bundled tool's arguments and result:** `tool_<name>_input` and
  `tool_<name>_output`, for `http_request`, `memory`, `delegate` and `chat`.
- **The plugin process protocol:** `tool_call_context` is what a tool call
  carries in `_meta.context`, `capability_request` is the parameters of a
  `capabilities/call` request, and `capability_record`, `capability_page`,
  `capability_http_response` and `capability_model_reply` are what comes
  back. The host checks every capability request against
  `capability_request`, bounds included, before it does anything.

## Validating a value

`schema::Validator` checks untrusted JSON against a schema, then converts it.
A refusal lists what failed, in a bounded form that can go back to a model.
