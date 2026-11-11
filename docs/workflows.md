# Workflows

A workflow is a fixed list of steps. Each step calls a tool or asks a sub-agent.
A model can decide what a step says, but not which steps run, so anything that
has to happen goes here, and not in a prompt that asks for it.

Start one from a [schedule](work.md) with `kind: workflow`:

```yaml
cron: "0 7 * * *"
kind: workflow
payload:
  workflow: read-and-remember
  inputs: {url: "https://example.com/"}
```

## Definitions

An agent is a file at `definitions/agents/<name>.yaml`
([schema](../schemas/v1/agent.schema.json)). A workflow is a file at
`definitions/workflows/<name>.yaml`
([schema](../schemas/v1/workflow.schema.json)). Both directories have an example
to copy.

```yaml
description: Reads a page, summarizes it, and stores the summary.
inputs:              # a strict JSON Schema
  type: object
  additionalProperties: false
  required: [url]
  properties:
    url: {type: string, maxLength: 2048}
steps:
  - type: tool
    id: fetch
    tool: http_request
    arguments: {url: "{{inputs.url}}"}
  - type: agent
    id: summary
    agent: summarizer
    message: Summarize the page at {{inputs.url}}.
    context: "{{steps.fetch.result.body}}"
output: "{{steps.summary.answer}}"
max_usd_micros: 0    # what the whole run may spend; 0 is the default
```

**A file is a definition's one home.** The worker reads them all at startup and
keeps a copy in the store that records which file it came from. A file that's
gone removes its copy. A file can't take the name of a definition that came from
somewhere else, and the worker won't start if one tries.

**Everything is checked before anything is saved.** One bad file stops the
worker and changes nothing. The checks are:

- the shape of each file, with unknown keys refused;
- that each tool and agent exists;
- each step's arguments against the tool's input schema;
- that step names are unique, and each reference is to a declared input or an
  earlier step;
- that a schedule's workflow exists, and its inputs fit.

## References

`{{inputs.name}}` is an input. `{{steps.id.result}}` is what a tool step
returned, and `{{steps.id.answer}}` is what an agent said. Go deeper with dots:
`{{steps.fetch.result.body}}`.

A string that is only a reference keeps the value's type, so a number stays a
number. Anywhere else, the value becomes text.

A reference to something that turns out to be missing or null fails the step and
names the reference. It's never replaced by an empty string. Filling in is one
pass: text a step returned can contain `{{` and it stays as text.

A value that's filled in can't be checked until the step runs, so a step's
arguments go through the tool's schema again then, as every tool call does.

## Agents

An agent is a chat with its own instructions and tools. It starts with nothing
but its system prompt and what the step gives it, and it doesn't search memory
first.

**What earlier steps found goes in `context`, never in `message`.** The worker
refuses a `message` that refers to a step. `context` is labelled as data for the
model, and the text inside it can't end that label. A `message` can use `inputs`.

An agent uses the brain's `budget` unless it sets its own. Either way, it can't
use more than what's left of the run's money and time. Models are chosen with
the purpose `workflow`, which is what a provider's `paid_for` matches.

## A run

Steps run in order. Each is its own dispatch session, so it has its own budget
and receipt, and what one step spends is taken from what the next can.

The first step that fails stops the run, and later steps are reported as
skipped. A step fails if:

- the tool is refused, fails or times out;
- the tool's result is over 500,000 characters, since a later step can't work
  from part of it;
- the agent doesn't finish, or its answer is cut off by its token limit;
- a reference is missing.

A run that finishes keeps this as the job's result
([schema](../schemas/v1/workflow_run.schema.json)): its output, its cost, and each
step's status and receipt. An output over 200,000 characters is cut, becomes
text and has `output_truncated` set. A run that fails is a failed job, and
`last_error` says which step and why. It keeps no result yet, so the rest is only
in the event log.

`workflow_time_limit_seconds` in `brain.yaml` limits a whole run, 1800 by default.
Like any job, a run isn't retried unless its schedule sets `max_attempts`, because
a retry repeats what the earlier steps did.

## Not yet

There's no branching, no loop, no `on_error` and no parallel step. A run can only
be started by a schedule.
