# Environment

Every variable starts with the product's name in capitals. The image and
`compose.yaml` set the paths, so with Docker you only need to set keys.

| Variable | Default outside Docker | What it is |
|---|---|---|
| `EXCOGITATE_MODELS` | `models.yaml` | [The models file](models.md). |
| `EXCOGITATE_CONFIG` | none | Optional settings. See `brain.example.yaml` and [`schemas/v1/brain_config.schema.json`](../schemas/v1/brain_config.schema.json). |
| `EXCOGITATE_DATA_DIR` | `data` | Where the store and the event log live. |
| `EXCOGITATE_DEFINITIONS` | `definitions` | Where schedule files live. See [work](work.md). |
| `EXCOGITATE_EXTENSION_DIR` | `.cache/duckdb-extensions` | DuckDB's extensions, from `scripts/fetch_duckdb_extensions.sh`. The image includes them. Nothing is downloaded at run time. |

Keys are read from whatever variable the configuration names:
`api_key_env` for a provider in `models.yaml`, and `env` for a
[credential](tools.md#configuration) in `http_request`. Put them in `.env`,
which is never committed and never copied into the image.

With Docker, `.env` is optional. A local model server needs no keys.
