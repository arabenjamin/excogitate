# MCP

`serve` speaks the Model Context Protocol on stdin and stdout, one JSON-RPC
message per line. Only MCP goes to stdout; everything else goes to stderr.
The same tools are [served over HTTP](api.md#mcp) by the `server` command.

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

## Recall

Before a chat's first round, the harness searches memory for the question.
Notes close enough in meaning join the transcript as a search already made, so
a small model has them without having to think of it. The receipt marks that
call `by_harness`, and it counts against the budget. If nothing is close
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
it. If the brain dies mid-call, that call's reservation stays on the books
until the day ends: it can overcount a day's spend, and never undercount it.
If the store can't be reached, priced calls are refused.
