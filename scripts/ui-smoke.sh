#!/usr/bin/env bash
# Build the server and run the headless Svelte UI/hot-path smoke suite.
# Delegates to v2-smoke.sh; defaults to the cut-over base path "/".
set -euo pipefail
exec "$(dirname "$0")/v2-smoke.sh" "${1:-/}"
