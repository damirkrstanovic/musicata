#!/usr/bin/env bash
# Regenerate the web client's TypeScript types from the Rust wire structs (ts-rs).
# Run after changing a `#[derive(ts_rs::TS)]` type in musicata-core. Output:
#   crates/musicata-server/web/src/types/*.ts
set -euo pipefail
cd "$(dirname "$0")/.."
TS_RS_EXPORT_DIR="$PWD/crates/musicata-server/web/src/types" \
  cargo test -p musicata-core --features ts --quiet
echo "Regenerated web types → crates/musicata-server/web/src/types/"
