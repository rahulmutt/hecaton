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
#
# Fail closed, and say what it means, if the guarded dependency ever leaves
# the core tree: `cargo tree -i` exits 101 on a package it cannot find, with
# a message about a package ID specification that reads like tooling
# breakage rather than "the thing you were guarding is gone".
if ! tree=$(cargo tree --workspace --edges features --invert reqwest) \
  || ! grep -q '^reqwest v' <<<"$tree"; then
  echo "no reqwest in the core dependency tree; update or drop this guard" >&2
  exit 1
fi

# Assert the whole feature set rather than denying a list of known-bad
# features: the set after the split is `json` alone (Spec H §10.1), so any
# future leak is covered without extending a denylist.
have=$(sed -n 's/^[^A-Za-z]*reqwest feature "\([^"]*\)".*/\1/p' <<<"$tree" | sort -u | paste -sd, -)
if [[ $have != "json" ]]; then
  echo "core reqwest features are '$have', expected 'json' (a plugin leaked into the core resolution)" >&2
  exit 1
fi
echo "core reqwest features are clean (json)"
