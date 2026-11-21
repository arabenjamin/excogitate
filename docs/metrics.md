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
route matched. The jobs and spend gauges are read from the store when
Prometheus scrapes. The worker counts each job attempt as it's recorded.

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
| `model_failures_total` | `model`, `reason` | Models a call passed over: failed (`rate_limited`, `timeout`, `unreachable`, `server_error`, `refused`, `bad_reply`) or left alone while `cooling` ([models](models.md#a-failing-model-is-left-alone-while-it-cools)) |
| `model_tokens_total` | `model`, `direction` | Tokens `input` and `output`, as the model counted them or as estimated |
| `model_call_duration_seconds` | `model` | Histogram, retries and fallbacks included |
| `capability_calls_total` | `capability`, `ok` | Calls plugins' tools made through a capability |
| `http_requests_total` | `route`, `status` | HTTP requests |
| `http_request_duration_seconds` | `route`, `status` | Histogram |
| `sink_append_failures_total` | | Event appends the sink failed: each is a piece of a trajectory lost |
| `jobs` | `state` | Jobs `queued` and `claimed`, counted up to 10,000 each |
| `jobs_finished_total` | `kind`, `workflow`, `outcome` | Job attempts ended: `succeeded`, `retried` (it will run again) or `dead` (its last attempt failed). `workflow` is the workflow a `workflow` job runs, `none` for other kinds |
| `spend_today_usd_micros` | `account`, `name` | What each priced provider (`account="provider"`) and each person (`account="person"`, `name` their id) has spent or reserved today, UTC |
| `day_cap_usd_micros` | `account`, `name` | Each one's day cap: a provider's `spend_cap_usd_micros_per_day`, a person's [own or the default](api.md#what-a-person-may-spend) |

A run whose agent isn't known when it ends, such as one that started before
a restart, is counted under `agent="unknown"`.

Counters start from zero when the server starts, as Prometheus expects;
`rate()` and `increase()` handle the reset. The event sink keeps the
history.

## What to alert on

- **Jobs dying:** `increase(jobs_finished_total{outcome="dead"}[1h])` by
  `workflow`. A source or sidecar that breaks shows here first, since a
  dead job only leaves the `jobs` gauge.
- **A tool failing most calls:** `tool_calls_total` with `status="failed"`
  over all of that tool's calls.
- **A cap filling:** `spend_today_usd_micros / day_cap_usd_micros` above
  0.8, where the cap isn't 0.
- **Lost events:** any increase in `sink_append_failures_total`.
