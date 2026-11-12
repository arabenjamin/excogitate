# Workflows

A workflow is a fixed list of steps. Each step calls a tool or asks a sub-agent.
A model can decide what a step says, but not which steps run, so anything that
has to happen goes here, and not in a prompt that asks for it.

Start one [over the API](api.md#workflows), or from a [schedule](work.md) with
`kind: workflow`:

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

**A file is a definition's one home.** The server reads them all at startup and
keeps a copy in the store that records which file it came from. A file that's
gone removes its copy. A file can't take the name of a definition that came from
somewhere else, and the server won't start if one tries.

**Everything is checked before anything is saved.** One bad file stops the
server and changes nothing. The checks are:

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

**What earlier steps found goes in `context`, never in `message`.** The server
refuses a `message` that refers to a step. `context` is labelled as data for the
model, and the text inside it can't end that label. A `message` can use `inputs`.

An agent uses Excogitate's `budget` unless it sets its own. Either way, it can't
use more than what's left of the run's money and time. Models are chosen with
the purpose `workflow`, which is what a provider's `paid_for` matches.

## The front door

Set `front_door` in `config.yaml` to an agent's name, and that agent answers
every chat: its instructions and its tools, in place of `system_prompt` and the
full tool list. Without it, a chat is offered every tool but `delegate`, which
stops working once plugins bring more than about 60 tools, and confuses a
small model well before that.

**`delegate` hands a task to another agent.** The front door lists it among
its tools. On every chat, the agents it can hand work to are added to its
instructions, each with its `description`, read fresh, so an agent added or
changed is there on the next chat. The agent runs as its own run, with its own
tools and budget, held to what the chat has left. It sees only the `task` and
the `context` it's handed, not the conversation. What it returns, its answer,
how its run ended and the run's id (readable at `/api/v1/runs/{id}`), is
labelled untrusted, and what it cost goes on the chat's receipt.

- **Give the front door `memory` too.** The harness's memory search before
  the first round (`recall_limit`) runs only for a run offered `memory`, and
  it's what lets a small model answer from what's stored; on a 4B local
  model, a front door with only `delegate` handed a stored fact on and lost
  it, and one with `memory` answered it every time.
- What's in `context` reaches the agent marked as data, with the task, and
  the agent is told to use its tools for anything it doesn't cover. An empty
  `context` is left out.
- An agent that has `delegate` itself isn't listed and is refused, so a
  hand-off can't loop.
- A hand-off counts as a write, since the agent may write. In a Harness²
  probe it still runs, as a probe of the agent it goes to: that agent's
  writes are stubbed and only allowed reads run.
- `delegate` isn't offered over MCP; a client reaches it through `chat`.
- A missing front door agent stops the server at startup.

The front door's runs are the agent's, so Harness² improves it like any other
agent. `definitions/agents/front-door.yaml.example` and
`researcher.yaml.example` are a pair to start from.

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

Every run keeps this as the job's result
([schema](../schemas/v1/workflow_run.schema.json)), whether it worked or not: its
output, its cost, and each step's status and receipt. An output over 200,000
characters is cut, becomes text and has `output_truncated` set. A run that fails
is a dead job, and `last_error` says which step and why.

`workflow_time_limit_seconds` in `config.yaml` limits a whole run, 1800 by default.
Like any job, a run isn't retried unless its schedule sets `max_attempts`, because
a retry repeats what the earlier steps did.

## Not yet

There's no branching, no loop, no `on_error` and no parallel step.
