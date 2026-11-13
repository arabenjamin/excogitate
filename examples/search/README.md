# search

Web search through [TinyFish](https://tinyfish.ai)'s search API: the
starting example of a plugin that calls an outside API. Part of the
showcase in `examples/`, where the Daily News Brief searches through it.

Stamped from the plugin template (`templates/plugin`); `.template.json`
records which version, so `scripts/plugin_template.sh update
examples/search` brings later template changes in.

## The tool

**`search_web`**: `query`, and optionally `news`, `sites`,
`exclude_sites`, `recency` (`day`, `week`, `month`, `year`) and `limit`.
It answers with the results (title, URL, snippet, and a date and publisher
when TinyFish gives them). A failure says what to do about it: check the
credential (401, 403), the daily allowance is spent (402), or slow down
(429).

The answer also has `engine` and `tried`, which is always empty here. A
search plugin that tries several engines can answer the same shape, so a
workflow written against this one runs on it unchanged.

## Configuring it

Grant it in `plugins.yaml`:

```yaml
version: 1
plugins:
  - {name: search, capabilities: [http]}
```

The plugin never holds the key. The runtime attaches it by host, from the
`http_request` configuration (`http:` in `config.yaml`, `docs/tools.md`),
and reads it from the environment:

```yaml
http:
  credentials:
    - { host: api.search.tinyfish.ai, header: x-api-key, env: TINYFISH_API_KEY }
```

TinyFish takes GET, so the default `allowed_methods` (`GET`, `HEAD`) serve:
nothing about the runtime's network rules is widened for it.

## What it shows

- A plugin that reaches the outside world only through the `http`
  capability: the runtime's address guard, size limits and credentials
  apply, and its calls count against the run.
- Results marked untrusted: what the web says is never instructions.
- A failure that's visible and says why, rather than an empty answer.
