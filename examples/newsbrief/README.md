# newsbrief

The Daily News Brief: each morning, every section of the brief is searched
for the last day's news, a writer agent turns the results into one brief,
and the brief is kept in memory. The plugin keeps the sections; the files
in [`definitions/`](definitions) say how they're used.

Stamped from the plugin template (`templates/plugin`); `.template.json`
records which version, so `scripts/plugin_template.sh update
examples/newsbrief` brings later template changes in.

## The parts

| Part | Where | What it does |
|---|---|---|
| Sections | this plugin, `newsbrief.section` records | What each section searches for, the sites it keeps to, its order |
| `newsbrief_sections` | this plugin | The sections in order, and today's date; fails when there are none |
| `newsbrief_section_set`, `newsbrief_section_remove` | this plugin | Add, replace or remove a section by name |
| `search_web` | [`../search`](../search) | One search per section: news, the last day |
| `newsbrief` workflow | `definitions/workflows/` | Sections, searches, the writer, then memory |
| `news-writer` agent | `definitions/agents/` | Writes the brief from the results, and only from them |
| schedule | `definitions/schedules/newsbrief.yaml.example` | Every morning at 07:00, once copied to `newsbrief.yaml` |

A plugin offers tools; definitions say which run, in what order. The
workflow is fixed steps, so the searches always happen: a model decides
what the brief says, not whether the sections get searched. A section
whose search fails is left out as "Nothing new found." rather than
failing the brief, and the run's receipt says which failed and why.

## Running it

With Docker (the compose file mounts `config.yaml` and `definitions/`):

1. **Build with both plugins:**
   `FEATURES=app/plugin-search,app/plugin-newsbrief docker compose build`.
2. **Grant them** in a `plugins.yaml` beside `compose.yaml`:
   ```yaml
   version: 1
   plugins:
     - {name: newsbrief, capabilities: [records]}
     - {name: search, capabilities: [http]}
   ```
   mount it with one more line under the service's `volumes`
   (`- ./plugins.yaml:/config/plugins.yaml:ro`), and name it in `.env`:
   `EXCOGITATE_PLUGINS=/config/plugins.yaml`.
3. **Give the search its key:** the credential in
   [`../search`](../search) goes under `http:` in `config.yaml`, and
   `TINYFISH_API_KEY` in `.env`.
4. **Add the definitions:** copy `definitions/workflows/newsbrief.yaml` and
   `definitions/agents/news-writer.yaml` into your own `definitions/`
   (and the schedule, as `newsbrief.yaml`, to run it each morning). Run
   from a checkout without Docker, `EXCOGITATE_DEFINITIONS=examples/newsbrief/definitions`
   reads them where they are.
5. **Start it** (`docker compose up -d`) and **add sections**, from a chat
   or any MCP client, each as the arguments of `newsbrief_section_set`:
   ```json
   {"name": "World", "query": "world news", "sites": ["apnews.com", "reuters.com"], "order": 1}
   {"name": "Science", "query": "science and space", "order": 2}
   ```
6. **Run it:** `POST /api/v1/workflows/newsbrief/runs` with `{"inputs": {}}`,
   or wait for the schedule. The job's result has the brief as its output,
   and each step's receipt; the brief is also a note tagged `newsbrief`,
   which later chats and runs find.

The workflow may spend nothing (`max_usd_micros: 0`), which suits a local
model. Raise it for a paid one.

## How it's tested

`tests/newsbrief.rs` runs the tools through the host. `tests/brief.rs` runs
the whole brief: the real runtime loads `definitions/` as they ship, the
workflow is started over REST and run by a worker, the model is scripted,
and `search_web` answers from a script through the search example's own
manifest, so the workflow is held to the real contract. It checks one
search per section, a failed search left out without failing the brief,
the brief as the run's output, and the brief found again in memory.
