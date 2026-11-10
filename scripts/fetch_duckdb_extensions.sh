#!/usr/bin/env bash
# Downloads the DuckDB extensions the store loads, into a fixed directory.
# Safe to re-run. DuckDB checks each extension's signature when it loads it.
# The version must match the one linked in crates/store-duckdb (a test checks).
set -euo pipefail

DUCKDB_VERSION=v2.0.0
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target=${1:-"$root/.cache/duckdb-extensions"}

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) platform=linux_amd64 ;;
  Linux/aarch64) platform=linux_arm64 ;;
  Darwin/arm64) platform=osx_arm64 ;;
  Darwin/x86_64) platform=osx_amd64 ;;
  *) echo "fetch_duckdb_extensions: unsupported platform $(uname -sm)" >&2; exit 1 ;;
esac

directory="$target/$DUCKDB_VERSION/$platform"
mkdir -p "$directory"
for extension in fts; do
  file="$directory/$extension.duckdb_extension"
  if [[ ! -s $file ]]; then
    url="https://extensions.duckdb.org/$DUCKDB_VERSION/$platform/$extension.duckdb_extension.gz"
    curl --fail --silent --show-error --location "$url" | gunzip > "$file.partial"
    mv "$file.partial" "$file"
  fi
done
