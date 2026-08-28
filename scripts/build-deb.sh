#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
builder_image="rust:1.92-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2"
cargo_deb_version="3.7.0"

mkdir -p "$repo_dir/.cargo" "$repo_dir/target" "$repo_dir/dist"

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/workspace/target \
  -v "$repo_dir/.cargo:/cargo" \
  -v "$repo_dir:/workspace" \
  -w /workspace \
  "$builder_image" \
  bash -c "
    set -euo pipefail
    cargo test --workspace --locked
    cargo install cargo-deb --locked --version '$cargo_deb_version'
    find dist -maxdepth 1 -type f -name 'slc-mcp_*.deb' -delete
    cargo deb --locked --package slc-mcp --output dist
  "

shopt -s nullglob
packages=("$repo_dir"/dist/slc-mcp_*.deb)
if [ "${#packages[@]}" -ne 1 ]; then
  echo "Expected exactly one slc-mcp package, found ${#packages[@]}" >&2
  exit 1
fi
sha256sum "${packages[0]}"
