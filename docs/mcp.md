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

## What a client gets

| Tool | What it does |
|---|---|
| `chat` | Asks Excogitate. It reads URLs and uses its memory, and returns `answer`, `stopped_because` and the full `receipt`. |
| `memory` | [The memory tool](tools.md), called directly. |
| `http_request` | [The fetch tool](tools.md), called directly. |
| `job_history` | [What the job queue ran](tools.md#job_history), the latest first. |

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

## Recall

Before a chat's first round, the harness searches memory for the question.
Notes close enough in meaning join the transcript as a search already made, so
a small model has them without having to think of it. The receipt marks that
call `by_harness`, and it counts against the budget. On the trajectory the
search comes after the instructions and the question it was made for. If
nothing is close
enough, the model sees nothing, because irrelevant notes make a small model
say it doesn't know things it does.

`recall_limit` sets how many notes (0 turns it off), and
`recall_min_similarity` how close they must be. The default of 0.6 suits
bge-m3 on short notes, so measure it for your embedder. With no embedder,
nothing passes that gate and the model can still search for itself.

Measured on a small local model, three tries per question, with 21 made-up
notes. Questions didn't ask it to search:

| | Stored facts | Plain questions | Tokens per chat |
|---|---|---|---|
| Nothing added | 20/66 | 27/30 | 950 |
| A prompt telling it to search first | 50/66 | 26/30 | 2020 |
| Recall with no gate | 66/66 | 20/30 | 1398 |
| Recall with the gate (the default) | 66/66, twice | 28/30, twice | about 1000 |

Part of the stored-fact set was held back from choosing the gate: 24 tries on
notes it wasn't tuned on all passed. It's one model and a small set of short
notes, so treat it as a direction. A stored instruction to reply with a fixed
word was never obeyed; the model sometimes quotes it.

The default system prompt tells the model that an empty memory isn't the same as
not knowing, and to store what it's asked to remember. Measured the same way,
on the same small model, against the earlier prompt that didn't say so:

| | Earlier prompt | Default prompt |
|---|---|---|
| Plain questions, with and without stored notes | 103/120 | 120/120 |
| Stored facts | 66/66 | 66/66 |
| Told to remember something, then asked for it | 25/30 | 29/30 |
| Told to read a page, and to store it | 18/18 | 18/18 |
| Tokens per chat, with stored notes | 1165 | 829 |

Personal questions with no answer in memory weren't invented in any of 42 tries
each. Plain questions held back from writing the prompt all passed. It's still
one model and small sets.

All work is private for now, so it only reaches `no_log` providers.

## Settings

[The variables are in env.md](env.md).

Settings cover each run's budget, the system prompt, reply and result sizes,
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
