#!/usr/bin/env bash
# Hot-path + flow smoke for the Svelte app. Builds the server, scans testdata, drives the
# app over CDP (tests/ui/v2-flows.mjs), and exits non-zero on any failed check.
# Usage: scripts/v2-smoke.sh [basePath]   (default /v2; use / after cutover)
set -euo pipefail
cd "$(dirname "$0")/.."

BASE_PATH="${1:-/v2}"
CHROME="${CHROME:-$HOME/.cache/ms-playwright/chromium-1217/chrome-linux64/chrome}"
PORT=3979

if [ ! -x "$CHROME" ]; then echo "no chromium at $CHROME; skipping"; exit 0; fi

cargo build -p musicata-server
TMP="$(mktemp -d)"
cleanup() { kill "${SRV:-}" "${CHR:-}" 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT

./target/debug/musicata-server --library testdata --database "$TMP/t.db" --addr "127.0.0.1:$PORT" >"$TMP/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 60); do
  curl -s "http://127.0.0.1:$PORT/api/albums?limit=1" 2>/dev/null | grep -q '"id"' && break
  sleep 0.4
done

"$CHROME" --headless=new --no-sandbox --disable-gpu --remote-debugging-port=9222 \
  --remote-allow-origins='*' --autoplay-policy=no-user-gesture-required about:blank >"$TMP/chrome.log" 2>&1 &
CHR=$!
for _ in $(seq 1 20); do curl -sf http://127.0.0.1:9222/json/version >/dev/null 2>&1 && break; sleep 0.3; done

echo "Svelte UI smoke ($BASE_PATH):"
node tests/ui/v2-flows.mjs "$PORT" "$BASE_PATH"
