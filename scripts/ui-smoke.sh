#!/usr/bin/env bash
# Build the server, then run the headless UI/lag smoke suite (tests/ui/run.mjs).
# Exits non-zero if any flow check fails. Skips (exit 0) if no Chromium is present.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p musicata-server
exec node tests/ui/run.mjs "$@"
