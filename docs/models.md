# Models

`crates/model` is the only way the brain calls a model. It works with any
endpoint on the OpenAI wire, local or cloud. Models are named in `models.yaml`
(start from `models.example.yaml`) and never in code; agents ask for
capabilities.

## How a model is chosen

Each model is checked in this order, and a model that fails is recorded with
the reason:

1. **Privacy.** Private work goes only to a provider with `no_log: true`.
   Nothing later can override this, so private work fails rather than leak.
2. **Capability.** `chat`, `tools`, `json_schema`, `embeddings` or
   `investigative`.
3. **Size.** The request and its reply must fit the context window.
4. **Permission to pay.** A priced model may only be used for a purpose its
   provider lists in `paid_for`.
5. **Budget.** The worst case must fit what the run has left.

The models left are tried cheapest first, and in configured order among equal
prices. Before a call to a priced model, its worst case is reserved against
the provider's daily cap, so concurrent calls can't overrun it together. The
call then settles at what it really cost, or at the worst case if the server
doesn't report usage.

If a call fails, the next model is tried, up to three, unless the server
said the request itself was bad. When no model can take a call, the error
names every model and why.

`MemoryLedger` forgets the day's spend on a restart. A deployment using a
priced model needs a durable ledger.

## Credentials

A key is read from the environment variable named by `api_key_env`, and
belongs to its provider, so nothing can send it elsewhere. Redirects aren't
followed, and a key can't be printed.

## Structured output

`generate_structured` returns a typed value. It sends the type's schema (the
server enforces it when the model has `json_schema`; otherwise the model is
told it in a message), takes the first complete JSON object from the reply,
and validates it. A refused reply gets one repair attempt, and then the call
fails, with both reasons.
