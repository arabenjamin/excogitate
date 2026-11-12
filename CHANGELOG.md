# Changelog

All notable changes are listed here. The file is generated from the
Conventional Commits by [git-cliff](https://git-cliff.org) (`cliff.toml`).

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

