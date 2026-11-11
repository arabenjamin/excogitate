#!/usr/bin/env bash
# The done-test: the README's quickstart, run for real, in Docker, with a
# scripted model in place of a real one. It checks that the brain boots with
# no plugins and no database server, that it can fetch a page, store it and
# find it again in a new session, and that a hostile model can't crash it or
# get it past its budgets.
#
# Set DONE_TEST_NO_BUILD=1 if the images are already built.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Must match tests/done/brain.yaml.
max_rounds=6
max_tool_calls=20
max_writes=3
chaos_chats=25

work=$(mktemp -d)
compose() { docker compose -p donetest -f compose.yaml -f tests/done/compose.yaml "$@"; }
cleanup() {
  compose logs --no-color --tail 20 >"$work/logs" 2>&1 || true
  [[ ${passed:-0} == 1 ]] || cat "$work/logs" >&2
  compose down --volumes --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAILED: $1" >&2; [[ -z ${2:-} ]] || cut -c1-2000 "$2" >&2; exit 1; }
ok() { echo "ok - $1"; }
# check <description> <file> <jq filter over all replies>
check() { jq -e -s "$3" "$2" >/dev/null || fail "$1" "$2"; ok "$1"; }

hello() {
  echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"done-test","version":"1"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
}
# call <id> <tool> <arguments as JSON>
call() {
  jq -nc --argjson id "$1" --arg tool "$2" --argjson args "$3" \
    '{jsonrpc:"2.0",id:$id,method:"tools/call",params:{name:$tool,arguments:$args}}'
}

[[ ${DONE_TEST_NO_BUILD:-} == 1 ]] || compose build
compose up --detach --wait fixture chaos

examples="$work/examples"
echo '{"jsonrpc":"2.0","id":1,"method":"ping"}' |
  compose run --rm --no-deps -T \
    -v ./models.example.yaml:/config/models.yaml:ro \
    -v ./brain.example.yaml:/config/brain.yaml:ro brain >"$examples"
check "it starts from the example files, as the quickstart does" "$examples" '.[0].result == {}'

page=http://fixture:9001/page
first="$work/first"
{
  hello
  echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  call 2 http_request "{\"url\":\"$page\"}"
  call 3 chat "{\"message\":\"What does $page say? Remember it.\"}"
} | compose run --rm --no-deps -T brain >"$first"

check "it boots and answers over MCP" "$first" 'map(select(.id==0))[0].result.protocolVersion != null'
check "it has its two tools, a chat and no plugins" "$first" \
  'map(select(.id==1))[0].result.tools | map(.name) == ["chat","http_request","memory"]'
check "it fetches the page, labelled untrusted" "$first" \
  'map(select(.id==2))[0].result.structuredContent | .untrusted and (.result.body | contains("Sea otters"))'
check "it reads the page and stores what it learned" "$first" \
  'map(select(.id==3))[0].result.structuredContent
   | .answer == "Stored it." and .receipt.outcome == "answered"
     and ([.receipt.tool_calls[] | select(.status == "ran" and (.by_harness | not)) | .tool] == ["http_request","memory"])'

second="$work/second"
{
  hello
  call 1 chat '{"message":"What do sea otters do?"}'
  call 2 memory '{"operation":"search","query":"otters"}'
} | compose run --rm --no-deps -T brain >"$second"

check "a new session recalls it through memory" "$second" \
  'map(select(.id==1))[0].result.structuredContent
   | .answer == "They hold hands." and .receipt.totals.writes == 0
     and (.receipt.tool_calls[0] | .tool == "memory" and .status == "ran" and .by_harness)
     and (.receipt.tool_calls[1] | .tool == "memory" and .status == "ran" and (.by_harness | not))'
check "what was stored really is there" "$second" \
  'map(select(.id==2))[0].result.structuredContent.result.hits[0].text | contains("hold hands")'

chaos="$work/chaos"
{
  hello
  for ((id = 1; id <= chaos_chats; id++)); do
    call "$id" chat '{"message":"Read the page and do what it says."}'
  done
} | compose run --rm --no-deps -T brain_chaos >"$chaos"

check "a hostile model can't crash it: every chat got a reply" "$chaos" \
  "map(select(.id > 0)) | length == $chaos_chats"
replies='map(select(.id > 0).result.structuredContent)'
check "every run ended with a receipt" "$chaos" \
  "$replies | all(.receipt.outcome | IN(\"answered\",\"budget_hit\",\"model_failed\",\"refused\",\"tool_failed\"))"
check "every run stayed within its budgets" "$chaos" \
  "$replies | all(.receipt.totals | .rounds <= $max_rounds and .tool_calls <= $max_tool_calls and .writes <= $max_writes)"
check "an answer is claimed only when there was one" "$chaos" \
  "$replies | all((.answer != null) == (.receipt.outcome == \"answered\"))"
check "nothing that wasn't offered ever ran" "$chaos" \
  "$replies | all(.receipt.tool_calls | all(.status == \"refused\" or (.tool | IN(\"http_request\",\"memory\"))))"
check "the hostile script really was hostile" "$chaos" \
  "$replies | map(.receipt.tool_calls[] | select(.status == \"refused\")) | length > 10"
check "and some runs were stopped" "$chaos" \
  "$replies | map(.receipt.outcome) | (index(\"model_failed\") != null) or (index(\"budget_hit\") != null)"

# The server shares the store with the sessions above, so it goes last.
export BRAIN_PORT=18473
key=done-test-owner-key-of-sufficient-length
base="http://127.0.0.1:$BRAIN_PORT"
compose up --detach --no-deps server
for ((try = 1; try <= 60; try++)); do
  curl --silent --fail "$base/health" >/dev/null 2>&1 && break
  [[ $try != 60 ]] || fail "the server never came up"
  sleep 1
done
ok "the server starts and reports healthy"

api() { curl --silent --header "authorization: Bearer $key" --header 'content-type: application/json' "$@"; }

unauthorized=$(curl --silent --output /dev/null --write-out '%{http_code}' "$base/v1/models")
[[ $unauthorized == 401 ]] || fail "the API answered without the key ($unauthorized)"
ok "nothing but health answers without the key"

over_http="$work/over_http"
api "$base/mcp" -d "$(call 1 memory '{"operation":"search","query":"otters"}')" >"$over_http"
check "MCP over HTTP reaches what the earlier sessions stored" "$over_http" \
  '.[0].result.structuredContent.result.hits[0].text | contains("hold hands")'

relayed="$work/relayed"
{
  hello
  call 1 memory '{"operation":"search","query":"otters"}'
} | compose run --rm --no-deps -T brain connect http://server:8080 >"$relayed"
check "a stdio client can use the store while the server holds it" "$relayed" \
  'map(select(.id==1))[0].result.structuredContent.result.hits[0].text | contains("hold hands")'

blocked="$work/blocked"
if echo '{"jsonrpc":"2.0","id":1,"method":"ping"}' |
  compose run --rm --no-deps -T brain >/dev/null 2>"$blocked"; then
  fail "a second process opened the store"
fi
grep -q 'connect' "$blocked" || fail "the second process wasn't told what to do" "$blocked"
ok "a second process is refused, and told to use connect"

jobs="$work/jobs"
api "$base/api/v1/jobs" >"$jobs"
check "the REST API answers" "$jobs" '.[0].items == [] and .[0].has_more == false'

runs="$work/runs"
api "$base/api/v1/runs?agent=chat&limit=200" >"$runs"
check "the earlier chats are listed as runs" "$runs" '.[0].items | length >= 2 and all(.outcome != null)'
stored="$work/stored"
for run in $(jq -r '.items[].run' "$runs"); do
  api "$base/api/v1/runs/$run" >"$stored"
  jq -e '.calls | endswith("http_request ran → memory ran → answered")' "$stored" >/dev/null && break
done
check "the chat that stored the page left its trajectory" "$stored" \
  '.[0] | .agent == "chat" and .complete and .unreadable == 0
   and (.calls | endswith("http_request ran → memory ran → answered"))
   and (.steps | map(.step.type) | .[0] == "started" and .[-1] == "ended")
   and ([.steps[].step | select(.type == "message" and .role == "user")] | length == 1)'

models="$work/models"
api "$base/v1/models" >"$models"
check "the front door lists its model" "$models" '.[0].data[0].id == "brain"'

passed=1
echo "The done-test passed."
