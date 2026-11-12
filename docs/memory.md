# Memory and retrieval

`crates/memory` stores notes and finds them again. The `memory` tool and
Excogitate's own loops both use it, through the store port.

## Storing

A note is split into chunks by estimated tokens, because embedding limits are
in tokens and dense text has about twice as many per character. Chunks break
at the largest boundary that fits: a line, a sentence, a word.

Each chunk is embedded through the model gateway, so routing, privacy and spend
caps apply. If a chunk can't be embedded, the note is still stored. The chunk
can then only be found by keyword, and `unembedded_chunks` says how many.

The same content returns the note that already has it, within one privacy
class. A public run storing words a private note already holds gets a note of
its own: it never learns the private one exists, and a private note never
depends on one public runs can change or delete.

Embedding is charged to the run that asked for it. The cost is on the result,
and each retry can spend only what the attempts before it left. A note that's
embedded and then not saved, because the store failed or another edit got
there first, still reports what the embedding cost.

## Searching

A search runs full text and vector retrieval over a pool wider than the answer,
fuses them by rank (reciprocal rank fusion), and returns each note once, with
its best chunk. Every result keeps its provenance, so an untrusted note stays
labelled when it comes back into a prompt.

When vectors aren't available, search uses keywords and `SearchMode` says why.
A half-blind search must not look like a healthy one.

## The eval

`eval/retrieval_golden.yaml` holds made-up notes and questions.
`crates/memory/tests/retrieval_eval.rs` runs them on the real DuckDB store, with
a stand-in embedder that knows meanings and is blind to codes and rare names.
It fails if hybrid recall or MRR falls below its floor, if fusion stops beating
either path alone, or if fusion buries what only one path can find.

It guards the pipeline against regressions. It says nothing about how a
particular embedding model performs.
