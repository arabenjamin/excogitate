# The API

The `server` command serves three things on one port, along with the workers and
the scheduler. `docker compose up -d` runs it.

| Path | What it is |
|---|---|
| `POST /mcp` | MCP over HTTP |
| `GET /v1/models`, `POST /v1/chat/completions` | An OpenAI-compatible front door |
| `/api/v1/...` | The REST API, including every tool at `POST /api/v1/tools/{name}` |
| `GET /api/v1/openapi.json` | This server's [OpenAPI](#openapi) document |
| `GET /health` | `{"status":"ok"}`, with no key needed |
| `GET /metrics` | [Prometheus metrics](metrics.md), keyed unless `server.open_metrics` is set |

## Access

**Everything but `/health` needs a key,** sent as
`Authorization: Bearer <key>`. There are two kinds:

- **The owner key,** in `EXCOGITATE_OWNER_KEY`. It has to be at least 32
  characters, and the server won't start without it. It may do everything,
  and it's the only key that manages people and keys. It isn't stored, so it
  can't be revoked or locked out; change the variable to change it.
- **A person's key,** `exk_<id>_<secret>`, made by the owner for one
  [person](#people-and-keys). Only the secret's SHA-256 is kept, and the
  secret is shown once, when the key is made. The `exk_` prefix lets a secret
  scanner find a leaked one.

A person has a **role**, and each of their keys has **scopes**. A request may
do what its key's scopes say, as far as the role allows. Both are checked on
every request, so a revoked key, a disabled person or a lowered role takes
effect at once.

| Scope | What it opens |
|---|---|
| `read` | Every `GET` under `/api/v1` (jobs, runs, definitions, Harness², this document), `GET /v1/models`, `/metrics`, and tool calls that only read |
| `chat` | `POST /v1/chat/completions`, the `chat` tool, and `PUT /api/v1/runs/{id}/feedback` |
| `run` | `POST /api/v1/workflows/{name}/runs`, and tool calls that change something |
| `admin` | `POST /api/v1/harness/attempts`, reverting a change, and holding or releasing an agent |

| Role | Scopes its keys may use |
|---|---|
| `viewer` | `read` |
| `member` | `read`, `chat`, `run` |
| `admin` | all four |

People and keys are the owner key's alone. Every route says what it needs in
one table (`NEEDS` in `crates/runtime/src/http/mod.rs`), and a route left out
of it is refused to every key, so a new route can't open by accident. The
[OpenAPI document](#openapi) gives each operation's scope.

A key that doesn't exist, has the wrong secret, or isn't a key at all is a
401 that doesn't say which, so a guess learns nothing. A revoked or expired
key, or one whose person is disabled, is a 401 that says so. A key without
the scope a route needs is a 403 naming the scope, or the role, if the key
has the scope but its person's role no longer allows it.

There's no rate limit per key. `server.max_concurrent_requests` is the limit
for everyone.

**There's no TLS.** Compose publishes the port to this machine only. Put a TLS
proxy in front before exposing it, or the key crosses the network in clear text.

**A request with an `Origin` header is refused** unless `server.allowed_origins`
in `config.yaml` lists that origin, and no CORS headers are sent. This keeps web
pages away from a server on your machine.

A body can be at most 8 MiB. Chats and tool calls run at most
`server.max_concurrent_requests` at a time (8 by default). Past that, the server
answers 503 with `Retry-After`, and doesn't queue. Once a chat or tool call
starts, it runs to the end of its budget even if the client hangs up, so it's
always recorded.

Requests are checked against [`schemas/v1`](../schemas/v1). A whole number
sent with a point, such as `2.0`, counts as an integer, as JSON Schema says.

**Every error is JSON.** The REST API, a refusal by the key check, and the
router's own refusals (a method a path doesn't take is a 405 with `Allow`, a
body over the limit a 413, an unknown path a 404) are
`{"kind", "message", "problems"}`
([schema](../schemas/v1/api_error.schema.json)). The front door's own errors
are in OpenAI's shape, and a malformed MCP message gets a JSON-RPC error; see
below.

## MCP

`POST /mcp`, one JSON-RPC message per request, answered with JSON. It offers the
same tools as stdio: [`chat`, `http_request`, `memory`, `job_history` and `calculate`](mcp.md). It keeps no
session and opens no event stream, so `GET` answers 405. A message that isn't
one JSON-RPC 2.0 object, or a `mcp-protocol-version` header other than
2025-06-18, is a 400 with a JSON-RPC error.

## The front door

Point any OpenAI client at `http://127.0.0.1:8080/v1`. It sees one model,
`excogitate`, and behind it all of Excogitate: memory, tools, budgets and routing. The
models that actually answer are the ones in `models.yaml`.

- Your `system` and `developer` messages are added after the writer's own
  instructions and don't replace them; only the writer sees them.
- `max_tokens` can only lower the server's own limit.
- Replies carry an extra `receipt` of what actually ran.
- With `stream: true`, the answer arrives as one chunk when the run ends. Excogitate
  doesn't stream tokens, and sends keep-alive comments while it works.
- A run that ends without an answer is a 502 of type `run_stopped`, with its
  receipt. While streaming, it's an `error` event.

Anything it can't honor is refused, not ignored: another `model`, `n` over 1,
non-empty `tools` or `tool_calls`, a tool message, a `response_format` other
than text, and content that isn't text. The request's schema says all of these
but the model, which the server's own document names. Other fields, such as
`top_p`, are ignored.

Its errors are OpenAI's: `{"error": {"message", "type", "param", "code"},
"receipt"}`, with `type` `invalid_request_error` for a request it refuses,
`run_stopped` (a 502, with the receipt) for a run that ended without an
answer, and `server_error` for its own failure. A request the key check or
the router refuses first gets the REST error.

## OpenAPI

[`openapi.yaml`](../openapi.yaml) is the OpenAPI 3.1 document of the core's
routes, generated by `schema-gen` from the same table the router is built
from, so no route goes undescribed. CI fails when it drifts, as it does for the
schemas.

**A running server serves its own document** at `GET /api/v1/openapi.json`
(with the key, like every route). It's built at start and lists each tool the
server offers at a path of its own, `/api/v1/tools/<name>`, with that tool's
input and output schemas, plugins' tools included. Its server URL is the host
it was asked on (and `X-Forwarded-Proto` if a proxy sets it). A REST client
such as Postman or Insomnia imports it from that URL. Import it again after
plugins change.

A test calls every route, its error replies included, and checks each reply's
status, content type and body against the server's own document.
An MCP message is held to the very schema the document gives it.

`scripts/contract_check.sh` runs Schemathesis (pinned with its dependencies by
hash, in `tests/contract/`) against a server alone on a network with no way
out: it calls every operation with generated requests and checks each reply
against the document. The done-test runs it with the same requests every time
(near enough: the stateful phase varies a little), so a failure on `main` is
the change's. A weekly workflow runs it with new requests and opens an issue
with the seed that repeats them (`CONTRACT_RANDOM=1 CONTRACT_SEED=<seed>`).
[`tests/contract/schemathesis.toml`](../tests/contract/schemathesis.toml)
holds the only two things it's told beyond the document, each with why: a body
over 8 MiB is refused with a 413 before its schema is read, and the front
door's last message must be the user's, which no JSON Schema can state.

A schema's own parts (`$defs`) become components of the document, with their
references pointed there. Two different parts with one name are told apart by
their schema's name as a prefix.

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
workflow and it answers 200 with the first run's job, without starting another,
for as long as that job is kept (`retention.job_days` after it finished).
A run isn't retried, because that would repeat what its earlier steps did.

### Jobs

`GET /api/v1/jobs?status=&limit=&offset=` lists jobs, newest first. `status` is
`queued`, `claimed`, `succeeded` or `dead`, and `limit` goes up to 200.

`GET /api/v1/jobs/{id}` is one [job](../schemas/v1/job_view.schema.json). `result`
is what it produced. For a workflow, that's the
[run report](workflows.md#a-run), including each step of a run that failed.

### Runs

Every chat, agent step and tool call is a run, with a receipt and a
[trajectory](dispatch.md#trajectories).

`GET /api/v1/runs?agent=&harness=&limit=&offset=`
([schema](../schemas/v1/run_list.schema.json)) lists runs, newest first. `agent`
is `chat`, `agent:<name>` or `tool:<name>`. `harness` is a harness id: runs with
the same one had the same instructions, tools and limits.

`GET /api/v1/runs/{id}` is the run's
[trajectory](../schemas/v1/trajectory.schema.json). The id is a receipt's `run`.

`PUT /api/v1/runs/{id}/feedback`
([schema](../schemas/v1/feedback_request.schema.json)) with
`{"mark": "bad", "note": "..."}` keeps the owner's word on a run, replacing
any it had, and answers with [what's kept](../schemas/v1/feedback.schema.json).
`mark` is `bad` or `good`. A run marked bad is evidence for
[Harness²](harness.md#what-to-improve) on its own, and its note is the
problem the editor is told about.

### Harness²

`POST /api/v1/harness/attempts` queues an attempt to improve an agent, or a
workflow with `"target": "workflow"`; `"routed": true` probes an agent as
the routed chat runs it ([routed attempts](harness.md#routed-attempts)), and
`GET /api/v1/harness/attempts` and `GET /api/v1/harness/attempts/{id}` read what
attempts found. See [Harness²](harness.md#running-one). The system adopts what
the evidence supports on its own; `GET /api/v1/harness/changes`,
`POST /api/v1/harness/changes/{id}/revert` and `/api/v1/harness/holds` are how the
owner sees and undoes it. See [adoption](harness.md#adoption).

### Tools

`POST /api/v1/tools/{name}` calls a tool, with its arguments as the body. It
makes the same call MCP's `tools/call` does, with the same tools, checks,
budget and receipt. A call that ran and failed is a 200 with
`{"is_error": true, "message"}`
([schema](../schemas/v1/tool_failure.schema.json)), because it ran. A tool
that doesn't exist is a 404, and arguments that don't meet its schema are a
400 with the problems, before anything runs.

### People and keys

Only the owner key reaches these. People are never deleted, so an actor on an
old record always resolves; disable one instead, and their keys all stop.

| Route | What it does |
|---|---|
| `PUT /api/v1/people/{slug}` | Adds a person (201) or changes one (200): `{"name", "role", "disabled"}` ([schema](../schemas/v1/person_request.schema.json)). Their id is `person:<slug>`, which is the actor on what they start. |
| `GET /api/v1/people?limit=&offset=` | The people. |
| `POST /api/v1/keys` | Makes a key: `{"person", "scopes", "name", "expires_at"}` ([schema](../schemas/v1/key_request.schema.json)). The reply has the `secret`, shown this once. A scope the person's role doesn't allow is a 403. A key doesn't expire unless `expires_at` is given. |
| `GET /api/v1/keys?person=&limit=&offset=` | The keys, never their secrets, each with `last_used_at` (written at most once a minute), so a stale one is easy to find. |
| `DELETE /api/v1/keys/{id}` | Revokes a key, at once. |

Any key reaches these, to see itself:

| Route | What it does |
|---|---|
| `GET /api/v1/me` | Who the key is: `actor`, `role`, the `scopes` it may use now, and its id ([schema](../schemas/v1/caller.schema.json)). |
| `GET /api/v1/me/keys` | Its person's keys. |
| `DELETE /api/v1/me/keys/{id}` | Revokes one of them, such as a key that leaked. |

From the command line, with `EXCOGITATE_OWNER_KEY` set, these ask the running
server (at `EXCOGITATE_URL`, or the address `EXCOGITATE_BIND` gives):

```sh
excogitate people put sam "Sam" member
excogitate keys make person:sam read,chat "Claude Code on the desktop"
excogitate keys person:sam
excogitate keys revoke <id>
```

In Compose, run them in the server's container:
`docker compose exec server excogitate keys …`.

### Definitions

`GET /api/v1/definitions/{kind}?limit=&offset=` and
`GET /api/v1/definitions/{kind}/{name}`, where `kind` is `agent`, `workflow` or
`schedule`. These are read-only: a definition's [one home](workflows.md) is its
file.
