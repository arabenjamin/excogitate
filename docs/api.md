# The API

The `server` command serves three things on one port, along with the workers and
the scheduler. `docker compose up -d` runs it.

| Path | What it is |
|---|---|
| `POST /mcp` | MCP over HTTP |
| `GET /v1/models`, `POST /v1/chat/completions` | An OpenAI-compatible front door |
| `/api/v1/...` | The REST API |
| `GET /health` | `{"status":"ok"}`, with no key needed |

## Access

**Everything but `/health` needs the owner key,** sent as
`Authorization: Bearer <key>`. Set it in `EXCOGITATE_OWNER_KEY`. It has to be at
least 32 characters, and the server won't start without it. There are no users
or roles yet, so whoever holds the key is the owner.

**There's no TLS.** Compose publishes the port to this machine only. Put a TLS
proxy in front before exposing it, or the key crosses the network in clear text.

**A request with an `Origin` header is refused** unless `server.allowed_origins`
in `brain.yaml` lists that origin, and no CORS headers are sent. This keeps web
pages away from a server on your machine.

A body can be at most 8 MiB. Chats and tool calls run at most
`server.max_concurrent_requests` at a time (8 by default). Past that, the server
answers 503 with `Retry-After`, and doesn't queue. Once a chat or tool call
starts, it runs to the end of its budget even if the client hangs up, so it's
always recorded.

Requests are checked against [`schemas/v1`](../schemas/v1).

## MCP

`POST /mcp`, one JSON-RPC message per request, answered with JSON. It offers the
same tools as stdio: [`chat`, `http_request` and `memory`](mcp.md). It keeps no
session and opens no event stream, so `GET` answers 405.

## The front door

Point any OpenAI client at `http://127.0.0.1:8080/v1`. It sees one model,
`brain`, and behind it the whole brain: memory, tools, budgets and routing. The
models that actually answer are the ones in `models.yaml`.

- Your `system` and `developer` messages are added after the brain's own
  instructions and don't replace them.
- `max_tokens` can only lower the server's own limit.
- Replies carry an extra `receipt` of what actually ran.
- With `stream: true`, the answer arrives as one chunk when the run ends. The
  brain doesn't stream tokens, and sends keep-alive comments while it works.
- A run that ends without an answer is a 502 of type `run_stopped`, with its
  receipt. While streaming, it's an `error` event.

Anything it can't honor is refused, not ignored: another `model`, `n` over 1,
non-empty `tools`, a tool message, a `response_format` other than text, and
content that isn't text. Other fields, such as `top_p`, are ignored.

## REST

An error is `{"kind", "message", "problems"}`
([schema](../schemas/v1/api_error.schema.json)). Unknown fields and query
parameters are refused.

### Workflows

`POST /api/v1/workflows/{name}/runs`
([schema](../schemas/v1/run_request.schema.json)):

```json
{"inputs": {"url": "https://example.com/"}, "priority": 0, "idempotency_key": "any-text"}
```

It answers 202 with `{"job_id", "already_existed"}`, once the workflow is known
to exist and the inputs to fit. Send the same `idempotency_key` for the same
workflow and it answers 200 with the first run's job, without starting another.
A run isn't retried, because that would repeat what its earlier steps did.

### Jobs

`GET /api/v1/jobs?status=&limit=&offset=` lists jobs, newest first. `status` is
`queued`, `claimed`, `succeeded` or `dead`, and `limit` goes up to 200.

`GET /api/v1/jobs/{id}` is one [job](../schemas/v1/job_view.schema.json). `result`
is what it produced. For a workflow, that's the
[run report](workflows.md#a-run), including each step of a run that failed.

### Definitions

`GET /api/v1/definitions/{kind}?limit=&offset=` and
`GET /api/v1/definitions/{kind}/{name}`, where `kind` is `agent`, `workflow` or
`schedule`. These are read-only: a definition's [one home](workflows.md) is its
file.
