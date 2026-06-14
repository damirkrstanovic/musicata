#!/usr/bin/env bash
# Headless UI smoke for the Svelte app, in two phases:
#   behavior — playback + flows against the light testdata fixture.
#   scale    — render/scroll/search against a read-only copy of the real ~11k-track DB
#              (--no-scan), if .musicata/musicata.db is present (else skipped).
# Exits non-zero if any check fails. Usage: scripts/v2-smoke.sh [basePath]  (default /v2).
set -euo pipefail
cd "$(dirname "$0")/.."

BASE_PATH="${1:-/v2}"
CHROME="${CHROME:-$HOME/.cache/ms-playwright/chromium-1217/chrome-linux64/chrome}"
REAL_DB="${MUSICATA_REAL_DB:-.musicata/musicata.db}"

if [ ! -x "$CHROME" ]; then echo "no chromium at $CHROME; skipping"; exit 0; fi

cargo build -p musicata-server
TMP="$(mktemp -d)"
SRV=""
cleanup() { kill "${SRV:-}" "${CHR:-}" 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT

# --mute-audio: the behavior phase actually plays tracks (real Web Audio), so mute the speakers.
"$CHROME" --headless=new --no-sandbox --disable-gpu --remote-debugging-port=9222 \
  --remote-allow-origins='*' --autoplay-policy=no-user-gesture-required --mute-audio about:blank >"$TMP/chrome.log" 2>&1 &
CHR=$!
for _ in $(seq 1 20); do curl -sf http://127.0.0.1:9222/json/version >/dev/null 2>&1 && break; sleep 0.3; done

# Run a phase: $1=mode, $2=port, $3..=server flags. Starts a server, waits for it, runs the
# flows, stops the server. Returns the flows' exit code.
run_phase() {
  local mode="$1" port="$2"; shift 2
  ./target/debug/musicata-server "$@" --addr "127.0.0.1:$port" >"$TMP/server-$mode.log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 80); do
    curl -s "http://127.0.0.1:$port/api/albums?limit=1" 2>/dev/null | grep -q '"id"' && break
    sleep 0.5
  done
  local rc=0
  node tests/ui/v2-flows.mjs "$port" "$BASE_PATH" "$mode" || rc=$?
  kill "$SRV" 2>/dev/null || true
  SRV=""
  return $rc
}

fail=0
run_phase behavior 3979 --library testdata --database "$TMP/behavior.db" || fail=1

if [ -f "$REAL_DB" ]; then
  cp "$REAL_DB" "$TMP/scale.db"
  cp "$REAL_DB-wal" "$TMP/scale.db-wal" 2>/dev/null || true
  cp "$REAL_DB-shm" "$TMP/scale.db-shm" 2>/dev/null || true
  # The throwaway copy may carry real user accounts; clear them so the flows' "set up a fresh
  # admin" auth path works (otherwise setup fails "already completed" and we can't sign in).
  sqlite3 "$TMP/scale.db" "DELETE FROM sessions; DELETE FROM users;" 2>/dev/null || true
  # Symlink the real artwork cache (sibling of the DB) so the scale server can serve covers
  # (the album artwork-render check) without copying ~hundreds of MB. Read-only, derived dir
  # name must match the server's `<db-dir>/artwork`.
  real_art="$(cd "$(dirname "$REAL_DB")" && pwd)/artwork"
  [ -d "$real_art" ] && ln -s "$real_art" "$TMP/artwork"
  run_phase scale 3978 --no-scan --database "$TMP/scale.db" || fail=1
else
  echo "Svelte UI smoke (scale): skipped (no $REAL_DB)"
fi

exit $fail
