{%- set kind = name | title -%}
# {{ name }}

{{ description }}

Stamped from the plugin template (`templates/plugin` in the runtime's
repo); `.template.json` records which version, so
`scripts/plugin_template.sh update` can bring later template changes in.

## What to write

- `plugin.yaml`: each tool's description and schemas (strict: objects
  closed, strings and arrays bounded), and each record type's schema.
- `src/lib.rs`: each tool's body, where it says TODO. A tool uses only the
  capabilities in `Granted`, which are the ones the manifest asks for.
- `tests/{{ name }}.rs`: a test for each thing a tool really does, through
  the host as the two there already do.

## Running it

- **Compiled in:** add `{{ kind }}Plugin` to the binary's plugin list, and
  grant it in `plugins.yaml`:
  `- {name: {{ name }}, capabilities: [{{ capabilities | replace(from=",", to=", ") }}]}`.
- **As its own process:** build the `{{ name }}-service` binary and give
  `plugins.yaml` a `process:` entry for it (see the runtime's
  `docs/plugins.md`).
