#!/usr/bin/env bash
# Regenerate the web client's TypeScript types from the Rust wire structs (ts-rs).
# Run after changing a `#[derive(ts_rs::TS)]` type in musicata-core. Output:
#   crates/musicata-server/web/src/types/*.ts
set -euo pipefail
cd "$(dirname "$0")/.."
export TS_RS_EXPORT_DIR="$PWD/crates/musicata-server/web/src/types"
# Core wire types (Track/Album/Player/…) and server response DTOs (admin: SourceView,
# AppSettings, ArtistAliasGroup, Activity). MUSICATA_SKIP_WEB_BUILD avoids the Vite build
# that the server crate's build.rs would otherwise run just to generate types.
cargo test -p musicata-core --features ts --quiet
# `snapcast` is included so the multi-room control DTOs (SnapClient, SnapcastStatus) export
# too; it only pulls in a pure-Rust crate (rubato), no system deps needed to generate types.
MUSICATA_SKIP_WEB_BUILD=1 cargo test -p musicata-server --features ts,snapcast --quiet
echo "Regenerated web types → crates/musicata-server/web/src/types/"
