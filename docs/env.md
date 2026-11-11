# Environment

Every variable starts with the product's name in capitals. The image and
`compose.yaml` set the paths, so with Docker you only need to set keys.

| Variable | Default outside Docker | What it is |
|---|---|---|
| `EXCOGITATE_MODELS` | `models.yaml` | [The models file](models.md). |
| `EXCOGITATE_CONFIG` | none | Optional settings. See `config.example.yaml` and [`schemas/v1/config.schema.json`](../schemas/v1/config.schema.json). |
| `EXCOGITATE_DATA_DIR` | `data` | Where the store and the event log live. With the Neo4j store, only the event log. |
| `EXCOGITATE_STORE` | `duckdb` | `duckdb`, or `neo4j` in a build with that feature. See [the store](store.md#the-neo4j-store). |
| `EXCOGITATE_NEO4J_URI` | none | With `STORE=neo4j`: the Bolt address, such as `bolt://neo4j:7687`. Required. |
| `EXCOGITATE_NEO4J_PASSWORD` | none | With `STORE=neo4j`: the password. Required. |
| `EXCOGITATE_NEO4J_USER` | `neo4j` | With `STORE=neo4j`: the user. |
| `EXCOGITATE_NEO4J_DATABASE` | the server's default | With `STORE=neo4j`: the database to use. |
| `EXCOGITATE_PLUGINS` | none | `plugins.yaml`: what each compiled-in plugin may use (schema: [`schemas/v1/plugins_config.schema.json`](../schemas/v1/plugins_config.schema.json)). Without it, nothing is granted. See [plugins](plugins.md#two-ways-to-run). |
| `EXCOGITATE_DEFINITIONS` | `definitions` | Where agent, workflow and schedule files live. See [workflows](workflows.md). |
| `EXCOGITATE_OWNER_KEY` | none | The `server` command won't start without it. At least 32 characters. See [the API](api.md). |
| `EXCOGITATE_BIND` | `127.0.0.1:8080` | Where the server listens. |
| `HOST_PORT` | `8080` | Compose only: the port on this machine. |
| `FEATURES` | none | Compose only: features to build in, such as `app/neo4j`. |
| `EXCOGITATE_EXTENSION_DIR` | `.cache/duckdb-extensions` | DuckDB's extensions, from `scripts/fetch_duckdb_extensions.sh`. The image includes them. Nothing is downloaded at run time. |

Keys are read from whatever variable the configuration names:
`api_key_env` for a provider in `models.yaml`, and `env` for a
[credential](tools.md#configuration) in `http_request`. Put them in `.env`,
which is never committed and never copied into the image.

With Docker, `.env` is optional. A local model server needs no keys.
