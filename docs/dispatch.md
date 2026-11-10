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
6. **Run**, in its own task, so a panic can't take the run with it, and
   within the run's time limit.
7. **Output.** The result must meet the tool's output schema. Then it's cut
   to `max_result_chars` if needed, and says so.

The model always gets a JSON reply it can act on: the result, a refusal with
what to fix, or a failure. Results from outside are labelled `untrusted`.

## Budgets

Every run has limits on rounds, tool calls, writes, tokens, money, request size
and time, with no defaults (`schemas/v1/budget.schema.json`). A refused call
still counts, so a loop of bad calls ends. Hitting one is final for the run.

## Receipts

`Session::finish` returns the `Receipt`, built only from what dispatch saw.
Every run ends in one outcome: `answered`, `refused`, `budget_hit`,
`tool_failed` or `model_failed`. A claim the record contradicts is corrected.

## Testing against a hostile model

`crates/chaos-model` is an OpenAI-wire server that plays a seeded script of
malformed bodies, wrong types, extra fields, huge strings, made-up tools,
false token counts and obeyed prompt injections. `crates/dispatch/tests/chaos.rs`
runs the harness against it through the real HTTP client, and checks that each
run stays within budget, that its receipt matches what really ran, and that
nothing not offered ever executes.

Parsers of model output also have property tests, and fuzz targets in `fuzz/`
that run weekly on a pinned nightly (`.github/workflows/fuzz.yml`).
