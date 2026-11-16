#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$(rustc -vV | sed -n 's/^host: //p')"
sidecar_dir="$repo_root/crates/desktop/binaries"

cargo build --manifest-path "$repo_root/Cargo.toml" --release -p cl5-server -p fake-console
mkdir -p "$sidecar_dir"
for binary in cl5-server fake-console; do
  sidecar="$sidecar_dir/$binary-$target"
  cp "$repo_root/target/release/$binary" "$sidecar"
  chmod +x "$sidecar"
  printf 'Prepared desktop sidecar: %s\n' "$sidecar"
done
