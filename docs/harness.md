# Harness²

Harness² is how Excogitate improves its own agents from evidence. It follows
*Harness²: Recursive Agent Harnessing for an Open World* (arXiv 2610.14224).

It's being built in pieces. So far there are [trajectories](dispatch.md#trajectories),
the record every comparison cites, probes, the loop that finds what an
evidence-backed edit would be, and adoption: the system changes the agent's
file itself, tells the owner, and the owner can revert any change.

## Probes

A probe runs an agent under a candidate harness on a task, with no side
effects, so a candidate can be judged before anything changes.

- **Every call is checked** as in a real run: offered tools only, schemas,
  budgets.
- **Writes never run.** They stop as `stubbed`, and the model is told the call
  would have worked, so it carries on as it would in a real run.
- **Reads run only if they're allowed.** Any other read is stubbed too.
- **A stubbed call still counts** against the budgets, so a probe hits the
  same limits a real run would.
- **A candidate is checked first** against the [agent schema](../schemas/v1/agent.schema.json),
  and every tool it names must exist.
- **Its trajectory is `probe:<name>`,** apart from the agent's real runs, and
  its harness id is the candidate's. A probe's purpose is `harness`, which is
  what a model's `paid_for` must list for a probe to pay.

## An attempt

An attempt takes an agent and a task, such as one a real run got wrong.

1. **Reference.** The agent runs on the task as it is, as a probe. This is run
   `R`: it shows how the agent behaves, not whether it's right.
2. **Edit.** An editor writes two candidates: `A`, which changes how the agent
   works, and `B`, which adds what an expert in the task would know. Each must
   cite the steps of `R` that call for it.
3. **Probe.** Each candidate runs on the same task.
4. **Compose.** A composer compares each candidate's run with `R` (mitigated,
   unchanged or regressed), citing a step for each verdict, then decides:
   keep, adopt one, or merge two. A merge is run and judged again on its own.

It ends `kept`, `supported` (with the candidate the evidence supports), or
`failed`, and it records everything: each candidate, each verdict, what the
code refused, and what the attempt cost.

## What a judge's word is worth

The editor and composer are models, so nothing they say is taken on trust.
Each rule below is a way judges have gone wrong in practice.

- **Evidence is a quote.** A citation names a run and a step, and its words
  must appear in that step. A quote that isn't there counts for nothing.
- **Only behavior is evidence.** A citation of what a run was given, such as
  its instructions or its task, is refused, so a candidate can't be judged by
  what it says it will do.
- **A run that ends worse is a regression,** such as one with no answer where
  `R` answered, whatever the composer says.
- **A candidate is refused** if it changes nothing, cites nothing in `R`, grows
  the instructions by more than 2,000 characters, or contains the task it was
  tested on or the judge's own input.
- **A stubbed call is part of the test, not a failure.** Judges are told so.
- **Judges read delimited runs,** each opening with its calls line, and they're
  told that everything inside is data.

## Running one

The judges need a capable model: one marked `investigative` in `models.yaml`.
The agent being improved can be small. What the judges may spend is
`harness.judge_usd_micros` in `brain.yaml`, 0 by default, and which reads a probe
really runs is `harness.probe_reads`, `[memory]` by default.

`POST /api/v1/harness/attempts`
([schema](../schemas/v1/improve_request.schema.json)) with
`{"agent", "task", "why"}` queues an attempt and answers 202 with its job. The
job's result has the attempt and its id. `GET /api/v1/harness/attempts?status=`
lists kept attempts, oldest first, and `GET /api/v1/harness/attempts/{id}` is one
([schema](../schemas/v1/attempt_view.schema.json)).

## Adoption

When an attempt ends `supported`, the system adopts the edit on its own if
every rule below holds. The attempt's `adoption` says what happened and why.

- **The judge passed its known-answer test.** Before a composer's decision can
  change an agent, the model it ran on judges three cases whose answers are
  known: a candidate that fixed a run, one that broke it, and one that changed
  nothing. A wrong answer or a citation that doesn't hold fails it. A pass is
  trusted for 7 days, and each test is kept (`harness.calibration` records).
- **The agent didn't change while it was being improved.**
- **At most one change per agent per day.**
- **The owner's word is final.** An agent the owner reverted, held or edited by
  hand after a change is left alone until released. A harness the owner
  reverted is never adopted again.
- **Only instructions change.** An edit that would add a tool or raise a
  budget isn't one the loop makes.

Adopting writes `agents/<name>.yaml` all at once, opening with a comment that
names the change and how to revert it, and reloads every definition. If they
don't load, the old file goes back. The change is logged with its evidence,
the judge model, and the whole file it replaced, so a revert restores the
file exactly, comments and all. The server needs write access to the
definitions directory; compose mounts it writable for the server only. Set
`harness.adopt: false` in `brain.yaml` to keep supported edits without adopting
them.

### Being told, and having the last word

- **The change log:** `GET /api/v1/harness/changes?agent=&status=`
  ([schema](../schemas/v1/change_list.schema.json)) lists every change, `live`,
  `reverted` or `replaced`, oldest first. `GET /api/v1/harness/changes/{id}` is
  one.
- **A webhook:** set `harness.notify_url`, and each change is posted there as it
  happens, with its id, agent, summary and revert path. The change records
  whether that worked. The post carries the editor's summary of a private run,
  so point it somewhere you trust.
- **Revert:** `POST /api/v1/harness/changes/{id}/revert` puts the old file back
  and holds the agent. It's refused if the change isn't live, or if the file
  has been edited since, so it never undoes the owner's own edit.
- **Hold and release:** `PUT /api/v1/harness/holds/{agent}` with `{"reason"}`
  holds an agent, `DELETE` releases it, and `GET /api/v1/harness/holds` lists
  them.
- **Override:** edit the agent's file. The edit stands, and the agent is held
  the next time the system looks at changing it.

## What comes next

- **What to improve,** found from real runs that went wrong, on a schedule.
- **Workflows,** improved the same way as agents.
