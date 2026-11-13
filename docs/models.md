# Models

`crates/model` is the only way Excogitate calls a model. It works with any
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
doesn't report usage. When the server reports what it billed, as a router
does, the call costs the higher of that and the listed price. A router can send
the same model to upstreams at different prices, and the listed price is only
the lowest: in one run, calls billed 1.2 to 6 times it. So a cap counts what was
billed, never less.

A call that fails after it may have reached the model (a timeout, or a reply
too large or malformed to use) may still be billed, so it settles at what its
prompt costs, the least such a call is billed. A refusal, a rate limit or a
request that never got through settles at nothing. Either way the failed
attempt's charge is on the run's receipt: added to the answer's cost when a
later model answers, and on the failed call when none does.

If a call fails, the next model is tried, up to three, unless the server
said the request itself was bad. When no model can take a call, the error
names every model and why.

Excogitate keeps this ledger in the store (`SpendLog`), so a restart can't reset
a cap. `MemoryLedger` forgets on a restart, and is only for tests.

## Asking for a capable model

The cheapest model that passes the checks answers, so a free local model
answers everything it can. Work that needs more asks for it by capability,
never by name:

- an agent, with `needs` in its definition: `needs: [investigative]`;
- a plugin's `model` calls, with `model_needs` in its grant in
  `plugins.yaml`. The deployment says it, never the plugin.

Only a model marked with each capability asked for in `models.yaml` is
chosen; if it's priced, its provider's `paid_for` still has to name the
purpose (`chat`, `workflow`, `plugin:<name>`), and its daily cap still
holds. Nothing else changes: work that asks for nothing still goes to the
cheapest.

## Credentials

A key is read from the environment variable named by `api_key_env`, and
belongs to its provider, so nothing can send it elsewhere. Redirects aren't
followed, and a key can't be printed.

## Structured output

`generate_structured` returns a typed value. It sends the type's schema (the
server enforces it when the model has `json_schema`; otherwise the model is
told it in a message), takes the first complete JSON object from the reply,
and validates it. A refused reply gets one repair attempt, and then the call
fails, with both reasons. An empty reply says so; a reasoning model can spend
its turn thinking and answer nothing.

Mark a model `json_schema` only once you've measured it that way. A server
that enforces a schema while it decodes can mangle the reply: in one run,
enforcement turned a short field into `", \"R` and dropped required lists, and
the same model got every field right when it was only told the schema.
