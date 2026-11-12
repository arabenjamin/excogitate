# Work that runs on its own

`crates/work` runs jobs without anyone waiting for them. The `server` command
runs the workers and the scheduler alongside [the API](api.md).

**DuckDB allows one process at a time,** and the server is that process. An MCP
stdio client can still [use it through `connect`](mcp.md#beside-the-server).

## Jobs

A job has a kind, a payload, a priority and `requested_by`. It's queued,
claimed, then succeeded or dead, and a dead job stays readable with
`last_error`. A job's `result` is what it produced.

- **The payload is checked** against its kind's schema before the handler runs.
- **A job can't outlive its lease.** The handler is stopped at its time limit,
  and the lease is longer than any limit. So a lease that lapses means the
  worker died, and the job is safe to claim again.
- **A retry waits longer each time.** Jobs that are out of attempts are dead.
- **A panic or a hang costs that job only.**

There are four kinds. A `chat` that doesn't end in an answer is a failed job,
so it can't look as though it worked. Chats from schedules pay as purpose
`schedule`, which is what a provider's `paid_for` matches. A `workflow` job runs
a [workflow](workflows.md), and one that stops at a failed step is a failed job.
A `harness` job is one [Harness²](harness.md) attempt, and a `harness_pick` job
chooses what to attempt.

A worker claims only the kinds it has a handler for, so a job meant for
another process waits for one that runs it. A schedule naming a kind nothing
here runs is refused when it loads, and the API only makes jobs of kinds it
runs.

Jobs move through `JobTransport`. The store is the default, and another queue
can take its place without changing the workers.

## Schedules

Each schedule is a file, `definitions/schedules/<name>.yaml`
(schema: [`schemas/v1/schedule.schema.json`](../schemas/v1/schedule.schema.json),
example: `definitions/schedules/morning-brief.yaml.example`).

```yaml
cron: "0 8 * * *"           # five fields, or six with seconds first
timezone: America/New_York   # UTC if left out
kind: chat
payload:
  message: What should I follow up on today?
max_attempts: 1
catch_up_seconds: 3600
```

The files are the one home of a schedule: they're read at startup, and a file
that's gone removes its schedule. Anything wrong with any file stops Excogitate
there, naming the file.

**A schedule fires once per slot.** The job's id comes from the schedule's name
and the time of the slot, so a restart or a second process adds nothing. There's
no "last run" to lose.

**If Excogitate was down,** it runs the latest missed slot, if that's within
`catch_up_seconds`. Older slots are skipped, and a schedule never runs a backlog.
It's at least 10 seconds, twice the scheduler's tick, so no slot falls between
two looks. A cron finer than the tick still fires at most once per tick.

A schedule defaults to one attempt, because a retry can spend money or repeat
what the first attempt did.
