# Changelog

All notable changes are listed here. The file is generated from the
Conventional Commits by [git-cliff](https://git-cliff.org) (`cliff.toml`).

## [0.29.2] - 2026-11-21

### Fixed

- The front door no longer takes a question about what was done, run,
  stored, learned or found for small talk; small talk is greetings, thanks,
  chatter and the conversation's own earlier messages

## [0.29.1] - 2026-11-21

### Fixed

- The server declares how people and keys are kept when it starts. Before,
  until a key was checked, `/metrics` left out every person's spend and a
  person's queued job read its day cap as $0

## [0.29.0] - 2026-11-21

### Features

- The metrics count how each job attempt ended (`jobs_finished_total`, by
  kind, workflow and outcome) and show each priced provider's and each
  person's spend today against its day cap; the worker gains a
  `JobObserver` hook

### Fixed

- An empty structured reply says when the model used its whole output limit

## [0.28.0] - 2026-11-21

### Features

- MCP is served in revisions 2026-07-28 (stateless), 2025-11-25 and
  2025-06-18, on stdio and over HTTP; `mcp-remote` speaks 2026-07-28 to a
  remote server that offers it and opens a handshake otherwise

### Build

- Bump `sha2` to 0.11, `jsonschema` to 0.61.2 and the CI actions

### Fixed

- The content hash is formatted as rustfmt does

## [0.27.0] - 2026-11-20

### Features

- Each person has a day cap on priced models, one total across providers
  inside each provider's cap; $0 unless set on the person or by
  `people.default_day_cap_usd_micros` (breaking for code that builds
  `RoutePolicy`, `EmbedScope` or `workflow::Engine` by hand, or matches
  every `Rejection`)

## [0.26.0] - 2026-11-20

### Features

- A person can keep a run to themselves
- What a private run stores is private too (breaking for store
  implementations: `ClassFilter` is a struct, and `find_note_by_hash` takes
  the note's privacy; the plugin contract only gains fields)

### Fixed

- The private-pick test takes its one line without indexing
- The front door's documented 502 is allowed in the contract check, and its
  image is rebuilt every run

## [0.25.0] - 2026-11-20

### Features

- Run subjects and session configs are made with constructors (breaking)

### Fixed

- A plugin stamped from the template compiles again

## [0.24.0] - 2026-11-20

### Features

- Every run names who asked for it

## [0.23.0] - 2026-11-20

### Features

- People, keys, roles and scopes

## [0.22.0] - 2026-11-19

### Fixed

- The OpenAPI document says what the server does (breaking)

## [0.21.0] - 2026-11-16

### Features

- An OpenAPI document, and every tool over REST

## [0.20.1] - 2026-11-16

### Fixed

- Published schemas keep only the formats JSON Schema defines

## [0.20.0] - 2026-11-16

### Added

- A daily review of answers, and a replay before adoption

## [0.19.0] - 2026-11-16

### Added

- The picker reads routed chats, and the owner can mark a run bad
- Routed attempts, a profile or the writer probed as the route runs it

## [0.18.0] - 2026-11-16

### Added

- Excogitate health, the image's health check

## [0.17.0] - 2026-11-16

### Added

- Every chat is a routed turn, and a profile may write (breaking)

## [0.16.0] - 2026-11-15

### Added

- A routed memory search is checked against the question

## [0.15.1] - 2026-11-15

### Fixed

- Calculate isn't a routed profile's first call

## [0.15.0] - 2026-11-14

### Added

- Memory counts notes, and calculate works out arithmetic (breaking)
- A routed turn about several subjects is split first

## [0.14.0] - 2026-11-14

### Added

- A routed turn shows each result, not dispatch's envelope

## [0.13.1] - 2026-11-14

### Fixed

- Grounding keeps a field the call requires through allOf

## [0.13.0] - 2026-11-14

### Added

- A routed turn only reads

## [0.12.0] - 2026-11-14

### Added

- A failing model is left alone while it cools
- Roles, the models for a job in the order to try them
- The routed front door, a line of workers (breaking)

## [0.11.0] - 2026-11-14

### Added

- Recent, the notes stored in a time window (breaking)
- Job_history, what the job queue ran

## [0.10.0] - 2026-11-13

### Added

- Prometheus metrics, counted from every run's events

## [0.9.0] - 2026-11-13

### Added

- Remote MCP servers' tools, offered as the runtime's own

## [0.8.0] - 2026-11-13

### Added

- An agent or a plugin's grant asks for a capable model

## [0.7.2] - 2026-11-13

### Added

- A tool step says how many capability calls its tool may make

## [0.7.1] - 2026-11-13

### Fixed

- The template's manifest no longer breaks a git dependency's build output

## [0.7.0] - 2026-11-13

### Added

- A TinyFish search plugin, the showcase's first
- A method for one host, and a key in the query (breaking)
- The Daily News Brief, end to end

### Fixed

- The search's publisher is the site's name

## [0.6.1] - 2026-11-12

### Added

- A plugin template, stamped and kept current by script

### Performance

- The Neo4j store searches vectors through its index

## [0.6.0] - 2026-11-12

### Changed

- Settle the published shapes before the freeze (breaking)
- One of each, and the tests the review asked for
- The store port and the event sink changed in 0.5.0 without saying so:
  `Archive` became part of `Store`, `find_note_by_hash` takes a class,
  `prune_jobs` and `EventSink::prune_runs` are required, `WriteOp::RestoreJob`
  and `Job.finished_at` are new. Both are internal before 1.0
  (docs/store.md) (breaking)

### Fixed

- The v0.5.0 review's bugs and holes
- The v0.5.0 review's drift, and failures made visible

## [0.5.0] - 2026-11-12

### Added

- Retention for trajectories and finished jobs
- **plugins:** Batch writes in the records capability
- **plugins:** The memory capability, and no capability writes in a probe
- **plugins:** The jobs capability, to start workflow runs
- **workflow:** A step can run once per item of a list
- **memory:** The lake hook, a plugin's sources read beside the store
- **agents:** Skills and guardrails in agent definitions
- **harness:** Workflows as a target; runs checked by action and by claim
- **store:** Export and import, everything a store holds as versioned JSONL

### Fixed

- Keep dedup within a privacy class, pin calibration, fit delegate's answer
- Count what failed calls and unsaved notes spent; order recall after the task
- A model call the run's time limit cuts off is on its receipt

## [0.4.0] - 2026-11-11

### Added

- **store:** A Neo4j store behind the neo4j feature
- **runtime:** Plugins in the running runtime, and a binary of your own
- **plugins:** The http and model capabilities
- **runtime:** A front door that delegates
- **plugins:** Process plugins with capabilities, on the stock image

### Changed

- Call the core the runtime, and the config config.yaml (breaking)

### Fixed

- The review's fixes before v0.4.0 (breaking)

## [0.3.0] - 2026-11-11

### Added

- **dispatch:** Record each run's trajectory and serve it over REST
- **workflow:** Probe an agent under a candidate harness
- **harness:** Find evidence-backed edits to an agent
- **harness:** Adopt supported edits on their own, with revert and holds
- **harness:** Pick what to improve from real runs that went wrong

### Fixed

- **store:** Finish the writer before a dropped store is gone
- **harness:** What the first real-model run found

## [0.2.0] - 2026-11-11

### Added

- **work:** Add the job workers and scheduler
- **workflow:** Add workflows with checked steps and sub-agents
- **api:** Serve MCP, an OpenAI-compatible front door and REST over HTTP

### Fixed

- **brain:** Tell the model an empty memory isn't ignorance; add connect

## [0.1.0] - 2026-11-11

### Added

- Start the repo with schemas and the store port
- **model:** Add the model port, routing and structured output
- **dispatch:** Add dispatch, receipts, budgets and the chaos model
- **plugins:** Add the plugin contract, host and example plugin
- **store:** Add the DuckDB store and event sink
- **memory:** Add notes, token chunking and hybrid retrieval
- **tools:** Add the memory and http_request tools
- **dispatch:** Add the chat round loop
- **brain:** Serve MCP over stdio
- **store:** Keep spend in the store so caps survive a restart
- Add the Docker image, quickstart and done-test
- **brain:** Search memory before a chat's first round

