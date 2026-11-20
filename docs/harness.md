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
   cite the steps of `R` that call for it. A candidate is the agent's
   instructions, and may also rewrite its [skills](workflows.md#skills) and
   add [guardrails](workflows.md#guardrails). The editor is told to prefer a
   skill to always-on text for what only some tasks need, and a guardrail
   when a run broke a rule the instructions already state.
3. **Probe.** Each candidate runs on the same task.
4. **Compose.** A composer compares each candidate's run with `R` (mitigated,
   unchanged or regressed), citing a step for each verdict, then decides:
   keep, adopt one, or merge two. A merge is run and judged again on its own.
   It keeps both candidates' skills and guardrails; where both have one of
   the same name, `A`'s is kept.

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
  the instructions by more than 2,000 characters or its skills by more than
  4,000, or contains the task it was tested on or the judge's own input,
  whether in its instructions, a skill, or a guardrail it adds.
- **A candidate may add guardrails, never remove or change one.** A rule the
  owner set can't be loosened by an edit, whatever the evidence.
- **A candidate is checked as an agent is** before it's run: its schema, its
  tools, and its skills' and guardrails' patterns.
- **A stubbed call is part of the test, not a failure.** Judges are told so.
- **Judges read delimited runs,** each opening with its calls line, and they're
  told that everything inside is data.

## Running one

The judges need a capable model: one marked `investigative` in `models.yaml`.
The agent being improved can be small. Measure a judge before you trust it;
the known-answer test is a floor, not a guarantee. In the first real runs, on
an agent whose prompt never said to use its memory, one free model wrote
garbage on any long prompt, another put placeholders where the instructions
belong, and a cheap one found and fixed the defect three times out of three
for well under a cent an attempt. In every run, nothing the judges got wrong
was adopted. What the judges may spend is
`harness.judge_usd_micros` in `config.yaml`, 0 by default, and which reads a probe
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
  nothing. A wrong answer or a citation that doesn't hold fails it. The test
  is pinned to the composer's model, so when the composer answered on a
  fallback, that fallback is what's tested. A pass is trusted for 7 days, and
  each test is kept (`harness.calibration` records).
- **The agent didn't change while it was being improved.**
- **At most one change per agent per day.**
- **The owner's word is final.** An agent the owner reverted, held or edited by
  hand after a change is left alone until released. A harness the owner
  reverted is never adopted again.
- **Only instructions change.** An edit that would add a tool or raise a
  budget isn't one the loop makes.
- **It holds up on other recent tasks.** An edit is tested on the one task it
  was made for, so before it's adopted it's replayed on up to
  `harness.replay_tasks` (3 by default, at most 8) other recent tasks of the
  same agent: from its own runs, or for a [routed attempt](#routed-attempts)
  from the chats it took part in. Each task runs with the agent as it is and
  with the edit, and the composer's verdict on the edit is checked as in the
  attempt. A regression, or a run that ended worse, stops adoption, and so
  does a replay that couldn't finish, since then nothing vouches for the edit
  beyond its one task. The attempt's `replay` has each task's runs and
  verdict. With no other recent tasks there's nothing to replay, and 0 turns
  the replay off. The replay's probes and judges are paid from the attempt's
  `harness.judge_usd_micros`.

Adopting records the change as `pending` first, then writes
`agents/<name>.yaml` all at once, opening with a comment that names the change
and how to revert it, and reloads every definition. Only then is the change
marked `live`. If the definitions don't load, the old file goes back and the
change is `abandoned`. If the runtime stops between the file and its record,
a `pending` change counts as live exactly when the agent's file is its own,
so no edit is ever on disk without a record. The file is staged under a
random name, synced to disk, and never written through a link.

An attempt runs within its job's time limit, less a part kept back (a fifth,
at most two minutes) for keeping it and adopting what it found. One that
runs out of time is kept as failed, saying so, and adoption is skipped when
too little time is left. The change is logged with its evidence,
the judge model, and the whole file it replaced, so a revert restores the
file exactly, comments and all. The server needs write access to the
definitions directory; compose mounts it writable for the server only. Set
`harness.adopt: false` in `config.yaml` to keep supported edits without adopting
them.

### Being told, and having the last word

- **The change log:** `GET /api/v1/harness/changes?agent=&status=`
  ([schema](../schemas/v1/change_list.schema.json)) lists every change,
  `pending`, `live`, `abandoned`, `reverted` or `replaced`, oldest first. `GET /api/v1/harness/changes/{id}` is
  one.
- **A webhook:** set `harness.notify_url`, and each change is posted there as it
  happens, with its id, agent, summary and revert path. The change records
  whether that worked. The post carries the editor's summary of a private run,
  so point it somewhere you trust.
- **Revert:** `POST /api/v1/harness/changes/{id}/revert` puts the old file back
  and holds the agent. It's refused if the change isn't in effect, or if the
  file has been edited since, so it never undoes the owner's own edit.
- **Hold and release:** `PUT /api/v1/harness/holds/{agent}` with `{"reason"}`
  holds an agent, `DELETE` releases it, and `GET /api/v1/harness/holds` lists
  them. For a workflow, add `?target=workflow` to the `PUT` or the
  `DELETE`. Releasing what nothing holds is a 404.
- **Override:** edit the agent's file. The edit stands, and the agent is held
  the next time the system looks at changing it.

## Workflows

A workflow is improved the same way. Start an attempt with
`{"agent": "<workflow>", "target": "workflow", "inputs": {...}}`, where
`inputs` is what to run it on, such as `{"topic": "otters"}`, checked
against the workflow's inputs schema. An agent's attempt takes a `task`
instead; a request with both, or with the wrong one, is refused.

- **An edit can change what a workflow asks, never what it does.** A
  candidate is a list of step edits: a new `message` or `context` for an
  agent step, or new `arguments` for a step whose tool only reads. A step
  left out stays as it is. Steps can't be added, removed, reordered or
  retyped, their tools and agents stay, and a step that calls a tool that
  writes is sent what it always was. The edited workflow must pass the
  checks a loaded one does, its steps' text may grow by at most 2,000
  characters, and no edited step may hold the inputs or the judge's input.
- **The probe runs every step as a probe.** Writes are stubbed, only the
  reads in `harness.probe_reads` run, and agent steps run as probes of their
  agents. A stubbed step's result is the stub, so a later step that reads a
  field the real write would have returned fails, in every run alike, and
  the judges compare what ran before it.
- **The judges read one run per probe:** each step's trajectory in order,
  its lines prefixed by the workflow step's id, then how the workflow ended
  and its output. A run that ends `failed` where `R` `completed` is a
  regression, whatever the composer says.
- **A merge is made in code,** not by the composer: `A`'s edits, with `B`'s
  filling in what `A` left alone, field by field. It's run and judged on its
  own.
- **Adoption, reverts, holds and lessons work as for an agent,** on
  `workflows/<name>.yaml`. Each record has `"target": "workflow"`. A
  workflow's hold is apart from an agent's of the same name, and a lesson
  about an agent is never shown to a workflow's editor, or the other way
  round.
- **`harness_pick` picks agents only.** A workflow's attempt is started over
  REST or as a `harness` job.

## The lesson library

Each adopted change leaves a lesson (`harness.lesson` records): the situation
it was made in (the task and the reported problem), what changed, as the
editor put it, and the evidence that supported it.

- **Only an adopted change teaches anything.** A lesson is written with the
  change in one store write, never from a verdict alone, so a judge's mistake
  can't enter the library.
- **A lesson stops counting** when its change is reverted, or when the owner
  edits the file after it.
- **It's looked up, never inherited.** When the editor works on an agent, it's
  shown at most three active lessons that fit: the agent's own first, then
  others whose words the task shares. They're labelled as hints, and an edit
  still needs evidence from the runs. The attempt records which it was shown,
  in `lessons_shown`.
- **`GET /api/v1/harness/lessons?agent=&status=`**
  ([schema](../schemas/v1/lesson_list.schema.json)) lists them; `status` is
  `active` or `retired`.

## What to improve

A `harness_pick` job, which takes `{}`, chooses an agent to improve from its
real runs, and queues one attempt for it. Put it on a
[schedule](work.md#schedules), as in
[`definitions/schedules/improve-agents.yaml.example`](../definitions/schedules/improve-agents.yaml.example).

- **It looks at each agent's last 20 runs from the past 7 days,** never at
  probes.
- **A run went wrong** if a tool call the model made failed, timed out or was
  refused, or if the run ended without an answer.
- **One failure is noise.** An agent is picked only when two or more runs went
  wrong the same way, and the agent with the most such runs wins.
- **The owner's word isn't.** A run marked bad with
  `PUT /api/v1/runs/{id}/feedback` ([API](api.md#runs)) is enough on its
  own, and the owner's note goes to the editor with the task.
- **Routed chats count too.** It reads the last 50 chats from the past 7 days
  and charges what went wrong to the profile it happened under: a call that
  failed, timed out or was refused, arguments that held values the question
  didn't, a call that would have run twice, or memory searches that found
  nothing that answers the question. A chat that ended without an answer, or
  one the owner marked bad, is charged to the writer.
- **What chats point to gets a routed attempt.** The profile or writer they
  point to is in the result's `routed`, and its attempt (`routed_job_id`)
  probes it as the route runs it (below). An agent picked from its own runs
  and from chats on the same day is attempted once.
- **The task is the newest of those runs' own question,** and `why` says how the
  runs went wrong.
- **An agent is left alone** if it's held, was tried in the last 24 hours, or was
  changed in the last 24 hours. Whatever it skips, and why, is in the job's
  result ([schema](../schemas/v1/pick_result.schema.json)).
- **At most one attempt per agent per day** is queued, even if a schedule fires
  twice.

## How it's tested

The HTTP tests and the [done-test](../scripts/done_test.sh) run the loop on a
scripted model, on every change: a candidate whose run ends worse is never
adopted even when the judge says it helped, a probe's write never happens, a
supported edit is adopted only after the judge passes its known answers, and a
revert puts the file back byte for byte. A run on real models is a manual check
before each release.

## Routed attempts

A profile or the writer doesn't converse, so probing it as a conversation
tests a way it never runs. An attempt with `"routed": true` probes it as
the routed chat runs it.

- **Each probe is a routed turn** on the task, with the route as it runs
  now and only the agent under test swapped for the candidate. Its
  trajectory is `probe:<name>`, and the judges read the route's steps: the
  choices, the arguments, the results and the answer.
- **Writes are stubbed, reads run.** A writer can only be judged on real
  results, so a routed probe runs every read its profiles make, apart from
  those in `harness.routed_probe_skips`: list the ones that cost money or a
  quota there. A profile's `chat_writes` doesn't let a probe's write run.
- **The judges are told the agent's part:** a profile's instructions only
  fill a tool's arguments, and the writer writes from the results.
- **A candidate may not add a guardrail.** An agent with one can't be a
  profile, so adopting it would drop the agent from the route.
- **Only an agent the route runs** can be attempted this way: an attempt on
  any other fails and says so, and a workflow is refused.

## Private runs

A person can keep a run [private](api.md#private-runs). Harness² still learns
from it, but never repeats its words: its messages, its model's replies and
its tools' results.

- **It counts.** A private run that failed counts toward "the same failure
  twice", by its failure pattern and calls line, which are tool names and
  statuses, and the owner's mark on it counts as always.
- **It's never the task.** An attempt's task is the newest run in the
  pattern that isn't private, and a replay never takes a private run's
  question. When every run in a pattern is private, nothing is attempted,
  and the picker's `skipped` says how many private runs failed that way.
- **No judge is shown one.** Judges read only an attempt's own probes, and
  a probe whose run is private is refused, so no citation can quote a
  private run, and nothing needs cleaning up afterwards.
- **The daily review skips it,** since a review quotes its chat, and says
  how many it skipped.

## Reviewing answers

An agent's run can end well and still answer badly, which no failed call
shows. A `harness_review` job, which takes `{}`, has a judge read the last
day's answered chats and say whether each answer holds up against its
results. Put it on a [schedule](work.md#schedules) before the pick, as in
[`definitions/schedules/review-answers.yaml.example`](../definitions/schedules/review-answers.yaml.example).

- **It reviews up to `harness.review_chats` chats,** newest first, within
  `harness.review_usd_micros` for the job. Both are 0 by default, which
  reviews none. A chat is reviewed once (`harness.review` records).
- **Each answer is `supported`, `unsupported` or `missed`:** everything it
  states is in a result; it states something no result holds; or a result
  holds something the question asked for, and the answer leaves it out.
  The judge quotes the step that shows it, and a quote that isn't there
  counts for nothing.
- **A fault is charged** to the writer for an unsupported answer, and for a
  missed one to the profile whose result was left out. The picker counts it
  as it counts a failed call: twice the same way picks the agent.

## What comes next

- **The picker for workflows:** choosing a workflow to improve from its
  failed runs, as `harness_pick` does for agents.
