#!/usr/bin/env bash
# gitleaks over the repository (default) or the staged changes (--staged).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/repo:ro" \
  zricethezav/gitleaks:v8.28.0 git /repo --redact --no-banner \
  ${1:+--pre-commit "$1"}
