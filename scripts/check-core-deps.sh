#!/usr/bin/env bash
# The daemon and client must not inherit a plugin's dependency features.
# `reqwest` in the core workspace is declared `default-features = false`
# with only `json`; anything else means a plugin leaked into the core
# feature resolution (Spec H §1, §4).
set -euo pipefail
cd "$(dirname "$0")/.."

# Workspace-wide: cargo unifies features across every package in one
# build, so the daemon links whatever reqwest the *workspace* resolves to,
# not what `-p hecaton-server` alone would resolve.
tree=$(cargo tree --workspace --edges features --invert reqwest)
bad=()
for feature in __tls __rustls http2 gzip stream; do
  if grep -q "reqwest feature \"$feature\"" <<<"$tree"; then
    bad+=("$feature")
  fi
done

if (( ${#bad[@]} )); then
  echo "core reqwest carries plugin-only features: ${bad[*]}" >&2
  exit 1
fi
echo "core reqwest features are clean"
