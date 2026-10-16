#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Build the same pinned, pipe-output Spotify Connect receiver for releases and Docker.
set -euo pipefail
output=$(realpath -m "${1:?output directory required}")
target=${2:-}
scripts=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
revision=d36f9f1907e8cc9d68a93f8ebc6b627b1bf7267d # librespot v0.8.0 (MIT)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$work" init -q
git -C "$work" remote add origin https://github.com/librespot-org/librespot.git
git -C "$work" fetch -q --depth 1 origin "$revision"
git -C "$work" checkout -q --detach FETCH_HEAD
test "$(git -C "$work" rev-parse HEAD)" = "$revision"
cd "$work"
features=rustls-tls-native-roots,with-libmdns
args=(--locked --release --bin librespot --no-default-features --features "$features")
build=(cargo build)
binary_dir=${CARGO_TARGET_DIR:-$work/target}
if [[ -n $target ]]; then
    build=(cargo zigbuild)
    args+=(--target "$target")
    binary_dir="$binary_dir/$target"
fi
"${build[@]}" "${args[@]}"
mkdir -p "$output"
cp "$binary_dir/release/librespot" "$output/librespot"
cargo metadata --locked --format-version 1 --no-default-features --features "$features" > "$work/metadata.json"
python3 "$scripts/librespot-notices.py" "$work/metadata.json" "$output/LIBRESPOT-NOTICES.txt"
