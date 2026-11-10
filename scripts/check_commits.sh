#!/usr/bin/env bash
# Checks commit messages against Conventional Commits.
#   check_commits.sh <base>..<head>   every commit in a range (CI, pull requests)
#   check_commits.sh --file <path>    one message (the commit-msg hook)
set -euo pipefail
re='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([a-z0-9._/-]+\))?!?: [^ ].*'
bad=0
check() { # $1 = subject, $2 = label
  case "$1" in Merge\ *|fixup!\ *|squash!\ *) return 0 ;; esac
  if ! [[ "$1" =~ $re ]]; then
    echo "not a Conventional Commit: $2 \"$1\"" >&2
    bad=1
  fi
}
if [ "${1:-}" = "--file" ]; then
  check "$(head -n1 "$2")" "message"
else
  while IFS= read -r line; do
    check "${line#* }" "${line%% *}"
  done < <(git log --format='%h %s' "$1")
fi
if [ "$bad" -ne 0 ]; then
  echo "Write type(scope): summary, e.g. 'feat(store): add the vectors table'. See CONTRIBUTING.md." >&2
  exit 1
fi
