#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$(rustc -vV | sed -n 's/^host: //p')"
source_binary="$repo_root/target/release/cl5-server"
sidecar_dir="$repo_root/crates/desktop/binaries"
sidecar="$sidecar_dir/cl5-server-$target"

cargo build --manifest-path "$repo_root/Cargo.toml" --release -p cl5-server
mkdir -p "$sidecar_dir"
cp "$source_binary" "$sidecar"
chmod +x "$sidecar"

printf 'Prepared desktop server sidecar: %s\n' "$sidecar"
