# Dispatch

`crates/dispatch` is the one path from what a model asks for to what actually
runs. A `Session` belongs to one run.

## What happens to a tool call

In order, and the first failure ends it:

1. **Budget.** Past a limit, nothing else runs.
2. **Offered.** The tool must be one this run was given. A real tool that
   wasn't offered is refused the same as one that doesn't exist.
3. **Size and form.** Arguments are capped, then parsed.
4. **Schema.** They must meet the tool's input schema; unknown keys are refused.
5. **Write budget**, if the tool writes.
6. **Probe.** In a [Harness² probe](harness.md#probes), a write, or a read
   that isn't allowed, stops here as `stubbed`. The model is told it would
   have run.
7. **Run**, in its own task, so a panic can't take the run with it, and
   within the run's time limit.
8. **Output.** The result must meet the tool's output schema. Then it's cut
   to `max_result_chars` if needed, and says so.

The model always gets a JSON reply it can act on: the result, a refusal with
what to fix, or a failure. Results from outside are labelled `untrusted`.

## The round loop

`Session::converse` is the loop: call the model, run what it asks for, send
back the results, repeat. Each round uses up the rounds budget, so it always
ends. It routes only to models that can use tools when the run has any.

- **An answer** is a reply with text and no tool calls. A reply cut off at
  the output limit is still an answer, and the receipt says `answer_cut_off`.
- **An empty reply isn't one.** The run ends as `model_failed` and says so.
- **A tool that fails** goes back to the model, which decides what to do.
- **The harness can make a call first** (`dispatch_for_harness`), with the
  same checks. The receipt marks it `by_harness`, and it won't take the last
  tool call a run has.
- **Hitting a budget** ends the run at once, with no answer and
  `stopped_because` saying which limit it was.

## Budgets

Every run has limits on rounds, tool calls, writes, tokens, money, request size
and time, with no defaults (`schemas/v1/budget.schema.json`). A refused call
still counts, so a loop of bad calls ends. Hitting one is final for the run.

What a tool spends counts too. It's told how much money, time and how many
tool calls are left, and its cost goes on the receipt, whether it worked or
failed. A tool with both reads and writes says which each call is, so only
real writes use the write budget. A plugin's `http` and `model` calls each
count as a tool call, and each is a `capability` step on the trajectory
under the tool call that made it.

A run handed to another agent with `delegate` gets no more time than its
caller has left, less a margin, so it ends first and its cost reaches the
caller's receipt.

When the server stops, every run still going ends at once, mid-call if need
be, as `budget_hit` with `stopped`, and keeps its receipt. A model call cut
off this way, or by a time limit, is settled in the day's spend at what its
prompt cost, and the run's receipt counts the same charge.

## Receipts

`Session::finish` returns the `Receipt`, built only from what dispatch saw.
Every run ends in one outcome: `answered`, `refused`, `budget_hit`,
`tool_failed`, `model_failed`, or `direct` for a single tool call made with
no model. A claim the record contradicts is corrected.

## Trajectories

A run's trajectory is everything it saw and did, in order: what ran and under
which harness, the messages the model started from, each model call and each
tool call with its arguments and reply, and how the run ended. It goes to the
event sink, apart from the store, one typed step per event
([schema](../schemas/v1/trajectory.schema.json)). Harness² will compare
trajectories, and a person can read one over [REST](api.md#runs).

- **A harness id** is a hash of what decides how a run behaves. For a chat,
  that's Excogitate's prompt, tools, budget and recall settings. For an agent,
  it's its instructions, tools and own limits. A client's own system message
  is input, so it's not part of the id. Two runs with the same id ran under the
  same harness.
- **What ran** is `chat`, `agent:<name>`, `tool:<name>`, or `probe:<name>` for
  a Harness² probe of an agent, so probes are never mixed up with real runs.
- **The calls line** sums a run up in one line, such as
  `memory ran (harness) → http_request refused → answered`. Read it first: it
  keeps runs from being confused with each other.
- **Long text is cut** at 16,000 characters and says so.
- **A lost event doesn't stop the run.** The receipt counts it in
  `events_lost`.
- **Trajectories hold private data,** the same as the run, so they aren't
  kept forever. The server removes whole runs, every event and the index
  entry together, `retention.trajectory_days` after they started: 30 by
  default, at least 8 so the week of runs Harness² reads is always there,
  and `null` to keep them. It sweeps at start and then hourly, and logs what
  it removed and anything that failed.

## Testing against a hostile model

`crates/chaos-model` is an OpenAI-wire server that plays a seeded script of
malformed bodies, wrong types, extra fields, huge strings, made-up tools,
false token counts and obeyed prompt injections. `crates/dispatch/tests/chaos.rs`
runs the round loop against it through the real HTTP client, and checks that each
run stays within budget, that its receipt matches what really ran, and that
nothing not offered ever executes.

Parsers of model output also have property tests, and fuzz targets in `fuzz/`
that run weekly on a pinned nightly (`.github/workflows/fuzz.yml`).
