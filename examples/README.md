# Examples: the Daily News Brief, end to end

A showcase of the runtime and its plugins doing one real job: a daily news
brief, each section searched for the last day's news, written up by an
agent from what was found, and kept in memory. Each plugin here was stamped
from the plugin template, the way yours would be, and each is compiled into
the app behind its own feature.

| Plugin | What it does |
|---|---|
| [`search`](search) | Web search through TinyFish, the key attached by the runtime |
| [`newsbrief`](newsbrief) | The brief's sections, and the workflow, agent and schedule that write it |

Start with [`newsbrief`](newsbrief): its README says how to run the brief,
and its `tests/brief.rs` runs the whole thing through the real runtime.

Plugins outside `examples/` and `plugins/example` aren't part of the
runtime; these show how one is built and used, and keep to the same
checks as the rest of the repo.
