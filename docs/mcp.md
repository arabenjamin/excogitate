# MCP

`serve` speaks the Model Context Protocol on stdin and stdout, one JSON-RPC
message per line. Only MCP goes to stdout; everything else goes to stderr.
The same tools are [served over HTTP](api.md#mcp) by the `server` command.

## Beside the server

DuckDB lets one process open the store, so `serve` fails with an error while the
server is running. For a stdio client, use `connect` instead. It opens nothing
and relays each message to the server, with the owner key from
`EXCOGITATE_OWNER_KEY`:

```sh
docker compose run --rm -T --no-deps session connect http://server:8080
```

The address defaults to `http://127.0.0.1:8080`. If the server can't be reached or
refuses the key, that message gets an error and the session carries on.

## What a key may call

Over HTTP, what a client may call depends on its [key's scopes](api.md#access):
`chat` needs `chat`, a call that only reads needs `read`, and a call that
changes something needs `run`. Some tools do both, depending on their
arguments: `memory` reads with `search` and writes with `store`, and
`http_request` reads with `GET`. Each call is judged by what it would do.

`tools/list` leaves out what the key can't call at all. A tool that can write
is still listed for a key with `read`, since such a tool often reads too. A
call the key may not make gets a JSON-RPC error, code `-32001`, naming the
scope it needs. It isn't reported as an unknown tool, so the reason is clear.

## What a client gets

| Tool | What it does |
|---|---|
| `chat` | Asks Excogitate, such as where a matter stands: a [routed turn](workflows.md#the-routed-front-door) looks up what it needs and a writer answers. Returns `answer`, `stopped_because` and the full `receipt`. `"private": true` keeps the run to whoever asked ([private runs](api.md#private-runs)). |
| `memory` | [The memory tool](tools.md), called directly. |
| `http_request` | [The fetch tool](tools.md), called directly. |
| `job_history` | [What the job queue ran](tools.md#job_history), the latest first. |
| `calculate` | [Arithmetic](tools.md#calculate), worked out exactly. |

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

[The variables are in env.md](env.md).

Settings cover each run's budget, the route and its writer's prompt, reply and result sizes,
and [`http_request`](tools.md#configuration). Money is off by default:
`max_usd_micros` is 0, so a priced model isn't used until you raise it. Chats
pay as purpose `chat`, direct calls as `tool`, and embedding as `memory`,
which is what a provider's `paid_for` matches.

The vector width is fixed when the store is first created. It comes from the
embedding model, or 1024 if there isn't one, so add the embedding model first
if yours is a different width.

A priced provider's daily cap is kept in the store, so a restart can't reset
it. If Excogitate dies mid-call, that call's reservation stays on the books
until the day ends: it can overcount a day's spend, and never undercount it.
If the store can't be reached, priced calls are refused.

## Remote servers

Excogitate is also a client: a remote MCP server's tools can be offered as
its own, to agents, workflows and chats, beside the bundled ones and
plugins'. The owner lists each server in `config.yaml`:

```yaml
mcp_servers:
  - name: docs                         # its tools are offered as docs_<tool>
    url: https://mcp.example.com/mcp   # Streamable HTTP; https unless allow_insecure
    header: authorization              # where its key goes, if it takes one
    prefix: "Bearer "
    env: DOCS_MCP_KEY                  # the variable holding the key
    no_log: false                      # true lets private runs call it
    tools: [search, fetch_page, save_note]
    read_only: [search, fetch_page]    # the rest are writes
    timeout_seconds: 30
    max_result_chars: 50000
```

At startup each server is listed, and each tool named becomes
`<name>_<tool>` (lower case, anything but letters, digits and `_` as `_`).
What a server offers beyond the tools named can't be reached. A tool named
that the server doesn't offer stops startup, saying what it does offer.

| Excogitate sets | To |
|---|---|
| Arguments | Checked against the server's input schema, made strict: an object with named properties is closed, a string gets at most 20,000 characters and an array 1,000 items where the server sets none, and a place that accepts anything (an open map) is left as the server's own. One that can't be made strict is refused at startup. |
| Effect | `read` for a tool in `read_only`, otherwise `write`: a probe never runs it, and it spends the run's write budget. |
| Privacy | A private run may call a server only if it's `no_log`. A public run may call any. |
| Addresses | The same guard as `http_request`: `http.blocked_ranges` and `http.allowed_ranges`, checked after DNS. A server on your own network needs its range allowed. No redirect is followed. |
| Key | Sent only to that server, in its header, from the variable named. If a server echoes it, it's replaced with `[redacted]`. |
| Result | Untrusted. `text` is its text content joined, with anything else it sent named in brackets; `structured` is its structured result when that fits; both are cut at `max_result_chars`, and `truncated` says so. A result the server marks as an error is a failed call, with its reason. |
| Time | `timeout_seconds`, or what the run has left if that's less. |

**Sessions.** A server that hands out a session id at `initialize` gets
it back on every message. One handshake is made per server and reused;
if the server forgets the session (a 404), a new one is started once. An
answer can be JSON or an event stream, and at most 8 MiB is read.

**A server that's down.** Each server's last good listing of the tools
named is kept in the store (as `mcp_client.listing` records, so no plugin
may be named `mcp_client`). If a server can't be listed at startup, its
kept listing is used, so its tools are still registered and the
definitions that name them still load; their calls fail, naming the
server, until it answers again, with no restart. A server that has never
been listed stops startup.

**What it doesn't stop.** A server marked `no_log` gets what a private run
sends it, as a provider marked `no_log` gets its prompts. A write tool
does whatever the server makes it do. Take only the tools you'd let a
model call, and leave out any that act for you somewhere you'd want to
approve first.
