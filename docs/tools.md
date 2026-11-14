# The bundled tools

Two tools come with Excogitate: `memory`, so a model can keep what it learns,
and `http_request`, so it has something to learn from. Everything else comes
from plugins. Both are in `crates/tools`, and both treat the model as an
adversary: its arguments say what it wants, and Excogitate decides where data
may go, who may see it and how far it can be trusted.

Each call's real effect goes on the receipt. `memory` counts as a read for
`search`, `recent`, `get` and `open`, and `http_request` for `GET` and `HEAD`. Anything else
counts against the run's write budget.

## `memory`

One flat argument object, with an `operation` of `store`, `search`, `recent`,
`get`, `update`, `delete` or `open`. The schema states which fields each one
needs. A search can set `min_similarity` to keep only notes close in meaning,
and each hit reports its `similarity`.

`recent` lists the notes stored in a time window, the latest first, for a
question with no query in it ("what did we keep today?"). The window is
`days` (the last so many, up to now) or `since`, and `until` ends it, not
included; `since` and `until` are RFC 3339 times or dates, a date meaning
its start in UTC. The answer gives the window it used, `total` (how many
notes it holds, listed or not) and `has_more`, and each note's start as
`excerpt`, marked `truncated`; `get` reads one whole. It lists 20 unless
`limit` says otherwise, at most 50.

A search also reads the sources plugins offer. Their items come back under
`sources`, ranked with the notes, so `score` compares across both; each says
which source it's from and its `id` there. `sources_skipped` names any source
left out because it failed, ran out of time or couldn't be searched by this
query. `open` reads one item whole, given its `source` and `item`, and
answers with `result: item`, its text marked `truncated` if it was cut.
Items from a source are always untrusted, and a result holding one is too.

| Excogitate sets | To |
|---|---|
| Privacy class | The run's. A model can't widen it. |
| What a run can see | Everything for a private run, only public notes for a public one. |
| Trust | Untrusted, for everything written through this tool. |
| Who stored it | `run:<id>` |
| Money | Embedding is charged to the run, and can't exceed what it has left. |

A note the owner wrote can't be changed or deleted through this tool, so a
prompt injection can't rewrite it. An `update` that changes nothing is a
failure. How search works is in [memory.md](memory.md).

## `http_request`

Arguments: `url`, `method` (default `GET`), `headers`, `body`, and `mode`
(`text` for a page's readable text, `raw` for the response as it came).

The result has the status, the final URL, the content type, the title, the
body, and whatever kept the answer from being whole: `truncated`,
`omitted_reason` (binary, or a page with no readable text), `decoding_errors`
and `redirect_not_followed`. It's always labelled untrusted. A 404 is a
successful call that reports a 404.

### What it defends against

- **Internal addresses.** Loopback, private, link-local and similar ranges are
  blocked, including IPv6 forms that carry an IPv4 address. Names are checked
  after DNS, on the addresses the connection will use, so a name can't
  rebind to something internal. Address literals are checked directly.
- **Redirects.** They're followed by hand, up to a limit, and each hop is
  checked. A model's headers stop at the origin it named. 301, 302 and 303
  become `GET` and lose the body.
- **Keys.** Excogitate attaches a key to requests for its exact host, from
  an environment variable the model never sees. A request carrying a key won't
  follow a redirect to another origin, and says so in the result. Keys go over
  https unless `allow_insecure` is set. If a response echoes a key, it's
  replaced with `[redacted]`.
- **Headers.** A model can't set `authorization`, `cookie`, `host`, `proxy-*`,
  a header that carries a configured key, or anything that changes how the
  request is framed or decoded.
- **Size and time.** Reading stops at `max_read_bytes`, the returned text at
  `max_body_chars`, and the whole call, redirects included, at
  `timeout_seconds`.
- **Hostile pages.** Readable text comes from an HTML tokenizer with no tree
  and no recursion, so its work grows with the page's length. It's fuzzed.

### What it doesn't stop

Remote MCP servers' tools go through the same address guard, with their
own rules: see [MCP](mcp.md#remote-servers).

**A run that can read private data and reach the web can send that data out,**
in a URL's query or path, even with only `GET`. Allowing `POST` widens that.
The defaults (`GET` and `HEAD`, and no keys) bound this, and don't remove it.
Run with less if the data matters: a public run, or a run without
`http_request`.

Text in a character set other than UTF-8 is decoded lossily, and
`decoding_errors` says when that happened.

### Configuration

The owner's, and validated at startup against
[`schemas/v1/http_config.schema.json`](../schemas/v1/http_config.schema.json).

```yaml
allowed_methods: [GET, HEAD]          # for every host
allowed_ranges: ["192.0.2.10/32"]     # reachable even though it's blocked
credentials:
  - host: api.example.com
    header: authorization
    prefix: "Bearer "
    env: EXAMPLE_API_KEY
    methods: [POST]                   # POST to this host only
  - host: search.example.net
    query: api_key                    # a service that takes its key no other way
    env: SEARCH_API_KEY
```

Leaving out `blocked_ranges` keeps the built-in list. Replacing it removes
the built-in list, and it can't be empty.

**A method for one host.** `allowed_methods` holds for every host, so
adding POST there lets a model send to anywhere. A credential's `methods`
allow more only for its own host: an API that takes only POST is reachable,
and the model's calls everywhere else stay GET and HEAD. The worst a model
can do with them is what that API allows with your key, such as spending
its quota. They don't follow a redirect off the host, since a request
with a key never does.

**Where the key goes.** A credential names a `header` (with an optional
`prefix`) or a `query` parameter, one of the two. A query key is added as
the request leaves, so the address the call keeps and returns never holds
it; it's scrubbed, as is a header's, from anything a response echoes back;
and a call that sets that parameter itself is refused. Prefer a header
where the service takes one: an address can end up in the service's own
logs.
