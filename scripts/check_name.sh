#!/usr/bin/env bash
# Fails when the product's name appears outside scripts/name-allowlist.
# The name is read from the binary target in crates/app/Cargo.toml, so this
# script never needs editing when the product is renamed.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
name=$(sed -n '/^\[\[bin\]\]/,/^\[/{s/^name *= *"\(.*\)"/\1/p}' crates/app/Cargo.toml | head -n1)
[ -n "$name" ] || { echo "check_name: no [[bin]] name in crates/app/Cargo.toml" >&2; exit 2; }
excludes=()
while IFS= read -r line; do
  case "$line" in ''|'#'*) continue ;; esac
  excludes+=(":(exclude,glob)$line")
done < scripts/name-allowlist
# --cached includes staged files; untracked files are not checked.
if hits=$(git grep -n -i -I --cached -e "$name" -- . "${excludes[@]}"); then
  echo "The name \"$name\" appears outside scripts/name-allowlist:" >&2
  echo "$hits" >&2
  echo "Use \"the runtime\" or a crate path, or add the file to the allowlist if the name belongs there." >&2
  exit 1
fi
echo "check_name: \"$name\" only where allowed"
