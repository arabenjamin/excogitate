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

## A plugin's tool in a step

A tool step is one call. A plugin's tool can make more through its
capabilities (`http`, `model`, `memory`, `jobs`), and each of those counts
as one of the step's tool calls, so a step allows one unless it says how
many:

```yaml
  - type: tool
    id: read
    tool: feeds_read
    arguments: {url: "{{inputs.url}}"}
    capability_calls: 40    # 1 to 200; 1 if left out
```

Past it, the plugin's next capability call is refused, and the step fails
saying the run has no tool calls left. With `for_each`, each item gets the
step's allowance. Records calls never count.

## Once per item

A tool or agent step with `for_each` runs once for each item of a list:

```yaml
  - type: tool
    id: read
    tool: http_request
    arguments: {url: "{{item.url}}"}
    for_each:
      items: "{{steps.find.result.links}}"   # one reference, to a list
      max_items: 20                          # required, 1 to 50; a longer list is cut
      continue_on_failure: false             # the default
```

- Each run sees its item as `{{item}}`, with dots to go deeper. Only a step
  with `for_each` has an item. An agent takes it in `context`, never in its
  `message`.
- Items run one after another, out of the step's money and time, so each gets
  what the ones before it left.
- The step's `result` (or `answer`) is a list, one entry per item that ran, in
  order. `{{steps.read.fan_out}}` says how many items there were (`total`), how
  many ran (`ran`), whether the list was cut (`cut`) and how many failed
  (`failed`).
- The first item that fails fails the step, unless `continue_on_failure` is
  set; then its entry is null and the rest run.
- In the run's result, the step has no receipt of its own. Each item has one,
  under the step's `items`.
- `items` that turns out not to be a list fails the step.

## Agents

An agent is a chat with its own instructions and tools. It starts with nothing
but its system prompt and what the step gives it, and it doesn't search memory
first.

**What earlier steps found goes in `context`, never in `message`.** The server
refuses a `message` that refers to a step. `context` is labelled as data for the
model, and the text inside it can't end that label. A `message` can use `inputs`.

**An agent says what its model must be able to do.** `needs:
[investigative]` sends its calls only to a model marked so in
`models.yaml` ([models](models.md#asking-for-a-capable-model)). Left out,
it gets the cheapest that can chat.

**An agent can ask for a role.** `role: writer` runs its calls on the
models `models.yaml`'s `writer` role lists, in that order
([roles](models.md#roles)), with its `needs` still applying to each. Left
out, it's routed by `needs`, cheapest first.

**An agent chooses what its memory reads.** `memory: {sources: [lake.notes]}`
limits the `memory` tool, in that agent's runs, to the sources named; an empty
list reads none, and left out, it reads every source the plugins offer, so a
narrow agent isn't flooded by a large lake. A source named that no plugin
offers stops the server, and so does naming sources without the `memory`
tool. The front door's choice holds for the whole chat.

### Skills

A skill is instruction text the agent gets only when the task calls for it,
so what only some tasks need isn't paid for on every run:

```yaml
skills:
  - name: citing
    when: '\b(sources?|cite)\b'
    unless: 'no sources'
    instructions: Name each source you used, with its address.
```

`when` and `unless` are patterns matched against the task as the agent
receives it (a step's `message` and `context`, a hand-off's task, or the
chat's newest message for the front door), ignoring case. A skill that
matches is added after the agent's instructions, under its name. An agent has
at most 16, each at most 4,000 characters. The run's trajectory says which
were loaded.

### Guardrails

A guardrail is a rule on one of the agent's tools that dispatch enforces, so
the agent doesn't have to remember it:

```yaml
guardrails:
  - name: no_deleting
    tool: memory
    because: Notes are kept; say which one is wrong instead.
    deny: {argument: operation, matches: '^delete$'}
  - name: few_fetches
    tool: http_request
    because: Three pages are enough for one answer.
    max_calls: 3
  - name: look_first
    tool: memory
    because: The answer is often already stored.
    required: true
```

- **`deny`** refuses a call whose argument matches the pattern. The argument
  is a dotted path, such as `filter.kind`. A string is matched as it is and
  anything else as JSON, ignoring case. A call without the argument is let
  through.
- **`max_calls`** refuses the agent's calls to the tool past that many.
- **`required`** means the agent must call the tool, and see it run, before
  it answers. An answer given first goes back once with a reminder, if the
  run has two rounds and a tool call left. If it answers again without the
  call, or there's no room for a reminder, the answer stands and the receipt
  lists the guardrail in `guardrails_unmet`.
- **`claimed`** is `required` for only the answers its pattern matches: what
  the answer says the tool did. `claimed: '\bI(.ve| have)? saved\b'` on
  `memory` means an answer that says it saved something needs the save to
  have run. It goes back once, as for `required`, told to call the tool or
  answer without the claim. A guardrail has `required` or `claimed`, not
  both.
- **`action: {argument, matches}`** narrows which calls of the tool the
  guardrail is about, matched as `deny` is: `{argument: operation, matches:
  '^store$'}` on `memory` counts only stores, so a search doesn't meet a
  guardrail that asks for a save, and `max_calls` caps only stores.

The answer is checked against what ran, not against what a model makes of
it, so the check is free and the same every time. What it can't see, such as
a claim worded some other way, it doesn't catch.

```yaml
guardrails:
  - name: saved_means_stored
    tool: memory
    action: {argument: operation, matches: '^store$'}
    claimed: '\bI(.ve| have)? (saved|stored)\b'
    because: Say you saved something only after memory's store has run.
```

A refused call tells the model `because`. Its receipt entry names the
guardrail and is `arguments_invalid` for `deny` or `budget` for
`max_calls`. A per-tool limit doesn't end the run. Only the agent's own calls
count, not the ones the harness makes for it, such as the front door's
memory search. A guardrail has at least one rule and holds a tool the agent
has. Names must be unique and patterns must compile, or the server doesn't
start. A workflow's `tool` steps have no guardrails; their arguments are
fixed in the definition.

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
labelled untrusted, and what it cost goes on the chat's receipt. An answer
over 200,000 characters is cut to that and comes back with
`answer_cut_off: true`; the whole answer is in the agent's run.

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

## The routed front door

`front_door` can instead give a route: a line of workers, where each step is
one small choice held to a closed set or to a schema and checked in code,
rather than one model holding every tool.

```yaml
front_door:
  profiles: [notes, jobs, web]   # agents; one to 24
  writer: writer                 # an agent with no tools
  max_calls: 3                   # tool calls per part, 1 to 6
  max_parts: 2                   # split a turn about several subjects; 1 to 4
  choice_role: choice            # roles in models.yaml; optional
  arguments_role: arguments
  only_when_asked:
    - {profile: web, asked: '\b(web|online|latest)\b', otherwise: notes}
```

A turn goes:

0. **Split.** With `max_parts` over 1 (it's 1 unless set), a model first
   says whether the turn asks about more than one subject, and if so
   writes each as a question that stands on its own, at most `max_parts`.
   Each part goes down the line below on its own, and the writer answers
   the whole turn from all their results. A turn about one subject runs on
   the owner's own words, not the model's rewording, and grounding always
   checks arguments against the owner's words, never a part's.
1. **Profile.** A model picks one of the profiles by their `description`, or
   `small_talk` (unless `small_talk: false`), from a closed list. A reply
   that names none of them is recorded as `invalid` and the turn looks
   nothing up.
2. **Stored first.** A profile in `only_when_asked` is taken only when the
   turn matches its pattern (case ignored); otherwise the turn goes to
   `otherwise`, so what's stored is read before the web.
3. **Tool.** A profile with one tool skips this. Otherwise a model picks one
   of its tools by their descriptions. `calculate` isn't offered as a first
   call when the profile has other tools: before anything has run there
   are no numbers for it.
4. **Arguments.** A model fills the tool's input schema, told the profile's
   instructions and skills, the question and the results so far. Then code
   grounds them: an optional string that isn't an enum value and appears in
   neither the question nor an earlier result is dropped, and so is any
   such item of a list of strings. A required field stays; the tool says if
   it's wrong. Arguments the same as a call already made are asked for once
   more, told what already ran; the same again ends the profile.
5. **Dispatch.** The call goes through dispatch like any other: the same
   checks, budget, trust labels and receipt. A turn only reads: a call
   that would change something, by what the tool says of these arguments
   (memory's `search` reads, its `delete` doesn't), isn't run, and the
   next choice sees that it was refused.
6. **Next.** A model sees each result, cut to 1,500 characters, and picks
   `done` or another of the profile's tools, until `max_calls`.
7. **Writer.** The writer agent answers from the question, the earlier turns
   and the results alone, with no tools, on its own `role` and `needs`. A
   turn that looked nothing up says so.

Each step sees a result as the tool returned it, not dispatch's envelope:
the labels (`untrusted`, `truncated`) stay on the receipt and the
trajectory, a cut result ends with how much of it is shown, and a refusal
or a failure is shown as it is. Shown the labels, a writer wrote about
them in place of the content.

No step is offered tools. Each choice uses the same pinned decision prompt,
and its reply must be one of the names. The turn is one run, `chat`, and its
trajectory records every choice (`choice`, with what was offered),
arguments kept and dropped (`arguments`), and each rule that fired
(`rule`: `split`, `invalid_choice`, `stored_first`, `reads_only`,
`no_repeat`, `max_calls`), so a
review sees why it went where it went. Its harness id covers the route,
each profile's agent and the writer, so changing any of them is a new
harness.

The route is checked when definitions load, and the server doesn't start
if it's wrong: each profile is an agent with a description and one to six
tools, none of them `delegate` and no guardrails, which a routed turn
doesn't run yet; the writer has no tools; `only_when_asked` names only its
profiles and its patterns compile; and the roles are defined.

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

There's no branching, no `on_error` and no parallel step. `for_each` is the
one loop, bounded, and its items run one at a time.
