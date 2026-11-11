# Changelog

All notable changes are listed here. The file is generated from the
Conventional Commits by [git-cliff](https://git-cliff.org) (`cliff.toml`).

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

