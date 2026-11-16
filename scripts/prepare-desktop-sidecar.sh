#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$(rustc -vV | sed -n 's/^host: //p')"
sidecar_dir="$repo_root/crates/desktop/binaries"
web_source_dir="$repo_root/crates/web"
web_dist_dir="$web_source_dir/dist"
packaged_web_dir="$repo_root/crates/desktop/resources/web"
launcher_source="$repo_root/crates/desktop/launcher.html"

cargo build --manifest-path "$repo_root/Cargo.toml" --release -p cl5-server -p fake-console
(cd "$web_source_dir" && NO_COLOR=true trunk build --release --dist dist)

test -f "$web_dist_dir/index.html"
find "$web_dist_dir" -type f -name '*.wasm' -print -quit | grep -q .
rm -rf "$packaged_web_dir"
mkdir -p "$packaged_web_dir"
cp -R "$web_dist_dir/." "$packaged_web_dir/"
cp "$launcher_source" "$packaged_web_dir/launcher.html"
test -f "$packaged_web_dir/index.html"
find "$packaged_web_dir" -type f -name '*.wasm' -print -quit | grep -q .
printf 'Prepared bundled web assets: %s\n' "$packaged_web_dir"

mkdir -p "$sidecar_dir"
for binary in cl5-server fake-console; do
  sidecar="$sidecar_dir/$binary-$target"
  cp "$repo_root/target/release/$binary" "$sidecar"
  chmod +x "$sidecar"
  printf 'Prepared desktop sidecar: %s\n' "$sidecar"
done
