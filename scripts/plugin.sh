#!/usr/bin/env bash
# Build, lint and test one standalone plugin project (Spec H).
#
# Each plugin is its own cargo workspace with its own lockfile, so every
# invocation runs from that plugin's directory. Its target directory is
# pinned here rather than inherited, because an ambient CARGO_TARGET_DIR
# would otherwise move the binary out from under package-plugins.sh.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"

usage() { echo "usage: $0 {target-dir|build|check} <name>" >&2; exit 2; }
[[ $# -eq 2 ]] || usage
cmd=$1
name=$2
dir="$repo/plugins/$name"
[[ -d $dir ]] || { echo "no such plugin: $name" >&2; exit 1; }
target="$dir/target"

case "$cmd" in
  target-dir)
    echo "$target"
    ;;
  build)
    CARGO_TARGET_DIR="$target" cargo build -q --manifest-path "$dir/Cargo.toml"
    ;;
  check)
    CARGO_TARGET_DIR="$target" cargo fmt --manifest-path "$dir/Cargo.toml" --all --check
    CARGO_TARGET_DIR="$target" cargo clippy --manifest-path "$dir/Cargo.toml" --all-targets -- -D warnings
    CARGO_TARGET_DIR="$target" cargo nextest run --manifest-path "$dir/Cargo.toml"
    ;;
  *)
    usage
    ;;
esac
