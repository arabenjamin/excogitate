# MCP

`serve` speaks the Model Context Protocol on stdin and stdout, one JSON-RPC
message per line. Only MCP goes to stdout; everything else goes to stderr.

## What a client gets

| Tool | What it does |
|---|---|
| `chat` | Asks the brain. It reads URLs and uses its memory, and returns `answer`, `stopped_because` and the full `receipt`. |
| `memory` | [The memory tool](tools.md), called directly. |
| `http_request` | [The fetch tool](tools.md), called directly. |

Every call is its own run, with its own budget and receipt, and goes through
the same [dispatch](dispatch.md) as a model's calls. A client is no more
trusted than a model is, so it can't set a privacy class or get around a
check.

- A chat that ended without an answer is still a successful call. Read
  `stopped_because` and the receipt.
- `isError` means the call itself was refused or failed.
- Listed schemas leave out conditions at the root, because some providers
  refuse them. They're still enforced, and a refusal says what was missing.
- Each chat stands alone. What carries over between them is what's in memory.

All work is private for now, so it only reaches `no_log` providers.

## Settings

Each variable starts with the binary's name in capitals and an underscore.

| Variable | Default | What it is |
|---|---|---|
| `MODELS` | `models.yaml` | [The models file](models.md). |
| `CONFIG` | none | Optional settings, checked against [`schemas/v1/brain_config.schema.json`](../schemas/v1/brain_config.schema.json). |
| `DATA_DIR` | `data` | Where the store and the event log live. |
| `EXTENSION_DIR` | `.cache/duckdb-extensions` | DuckDB's extensions, from `scripts/fetch_duckdb_extensions.sh`. Nothing is downloaded at run time. |

Settings cover each run's budget, the system prompt, reply and result sizes,
and [`http_request`](tools.md#configuration). Money is off by default:
`max_usd_micros` is 0, so a priced model isn't used until you raise it. Chats
pay as purpose `chat`, direct calls as `tool`, and embedding as `memory`,
which is what a provider's `paid_for` matches.

The vector width is fixed when the store is first created. It comes from the
embedding model, or 1024 if there isn't one, so add the embedding model first
if yours is a different width.

The daily spend cap is kept in memory for now, so a restart forgets the day's
spend. The brain says so at startup if a priced model is configured.
