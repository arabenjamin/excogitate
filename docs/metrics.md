# Metrics

`GET /metrics` serves Prometheus metrics in the OpenMetrics text format. Each
name starts with the binary's name: `excogitate_` for the stock binary, or
your own binary's name for a deployment that builds one.

## Access

**It needs the owner key by default,** like everything but `/health`, because
the metrics name every agent, tool and model the server has run. Prometheus
sends the key from a file:

```yaml
scrape_configs:
  - job_name: excogitate
    authorization:
      credentials_file: /etc/prometheus/secrets/excogitate.key
    static_configs:
      - targets: ["127.0.0.1:8080"]
```

On a private network you can open it instead, in `config.yaml`:

```yaml
server:
  open_metrics: true
```

Only `/metrics` opens. Everything else still needs the key.

## Where the numbers come from

Every run already writes its events: when it starts, each model call, each
tool call, each capability call a plugin's tool makes, and how it ends. The
event sink is wrapped, and each event is counted on its way through, so
the numbers and the trajectories in `GET /api/v1/runs` come from the same
events. An event that doesn't parse as a step is passed through and left
uncounted.

The HTTP layer times each request under its route's pattern
(`/api/v1/runs/{id}`, never the path asked for), and `other` for a path no
route matched. The jobs gauge is read from the store when Prometheus
scrapes.

**Labels never carry a run id, an argument or a reply.** Their values are
agent, tool, model and capability names. Each label keeps at most 256 values,
128 characters each, and counts the rest under `other`, so a stream of odd
names can't grow the series without bound.

## The metrics

Counters end in `_total`.

| Metric | Labels | What |
|---|---|---|
| `runs_started_total` | `agent` | Runs started: `chat`, `agent:<name>`, `tool:<name>` |
| `runs_total` | `agent`, `outcome` | Runs ended, and how (`answered`, `refused`, `budget_hit`, `tool_failed`, `model_failed`, `direct`) |
| `run_cost_usd_micros_total` | `agent` | What ended runs cost, in millionths of a dollar |
| `run_budget_hits_total` | `agent`, `kind` | Runs stopped by a limit, by the limit |
| `tool_calls_total` | `tool`, `status`, `effect` | Tool calls, what became of each, and whether the tool reads or writes |
| `tool_call_duration_seconds` | `tool` | Histogram, 10 ms to about 11 minutes |
| `tool_refusals_total` | `tool`, `kind` | Calls refused, by why |
| `model_calls_total` | `model`, `ok` | Model calls. `model` is `none` when no model answered |
| `model_tokens_total` | `model`, `direction` | Tokens `input` and `output`, as the model counted them or as estimated |
| `model_call_duration_seconds` | `model` | Histogram, retries and fallbacks included |
| `capability_calls_total` | `capability`, `ok` | Calls plugins' tools made through a capability |
| `http_requests_total` | `route`, `status` | HTTP requests |
| `http_request_duration_seconds` | `route`, `status` | Histogram |
| `sink_append_failures_total` | | Event appends the sink failed: each is a piece of a trajectory lost |
| `jobs` | `state` | Jobs `queued` and `claimed`, counted up to 10,000 each |

A run whose agent isn't known when it ends, such as one that started before
a restart, is counted under `agent="unknown"`.

Counters start from zero when the server starts, as Prometheus expects;
`rate()` and `increase()` handle the reset. The event sink keeps the
history.
