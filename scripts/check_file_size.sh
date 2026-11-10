#!/usr/bin/env bash
# Keeps the repository lean: no file over the limit gets in. With --staged it
# checks what's about to be committed; otherwise, everything tracked.
set -euo pipefail

limit_bytes=$((1024 * 1024))
failed=0

check() { # path, size
  if (($2 > limit_bytes)); then
    echo "check_file_size: $1 is $2 bytes; the limit is $limit_bytes" >&2
    failed=1
  fi
}

if [[ ${1:-} == --staged ]]; then
  while IFS= read -r -d '' path; do
    check "$path" "$(git cat-file -s ":$path")"
  done < <(git diff --cached --name-only --diff-filter=ACMRT -z)
else
  while read -r _ _ _ size path; do
    check "$path" "$size"
  done < <(git ls-files -z | xargs -0 -r git ls-tree -l HEAD -- 2>/dev/null)
fi

exit "$failed"
