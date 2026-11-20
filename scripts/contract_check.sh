#!/usr/bin/env bash
# The contract check: a server from excogitate:local, alone on a network with
# no way out, and Schemathesis calling every operation in the document it
# serves, checking each reply's status, content type and body against it.
# tests/contract/schemathesis.toml holds the two things a document can't say.
#
# The requests are the same every run, so a failure is the change's. With
# CONTRACT_RANDOM=1 they're new each time, as the weekly run does, and the
# seed printed at the end repeats them as CONTRACT_SEED. CONTRACT_EXAMPLES is
# how many each operation gets (25).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

compose() {
  docker compose -p contractcheck -f compose.yaml -f tests/done/compose.yaml --profile contract "$@"
}
# Only the contract services are used, so the done-test's own settings
# needn't point anywhere real.
export DONE_DEFINITIONS=${DONE_DEFINITIONS:-$(mktemp -d)}
trap 'compose down --volumes --remove-orphans >/dev/null 2>&1 || true' EXIT
docker image inspect excogitate-contract:local >/dev/null 2>&1 || compose build contract
mode=$([[ ${CONTRACT_RANDOM:-} == 1 ]] && echo "new requests${CONTRACT_SEED:+, seed $CONTRACT_SEED}" || echo "the fixed requests")
echo "contract: $mode, ${CONTRACT_EXAMPLES:-25} examples an operation"
compose run --rm contract
