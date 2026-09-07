#!/usr/bin/env bash
# Assembles each in-tree plugin as a directory source under target/plugins/
# (plugins spec §17.6): the crate's package/ files plus the freshly built
# binary in bin/. The e2e names these directories in plugins.yaml; by hand,
# so can you. One loop: a new in-tree plugin is one more name.
set -euo pipefail
cd "$(dirname "$0")/.."

for name in flow; do
  crate="hecaton-plugin-$name"
  cargo build -q -p "$crate"
  out="target/plugins/$name"
  rm -rf "$out"
  mkdir -p "$out/bin"
  cp "target/debug/$crate" "$out/bin/$crate"
  cp "crates/$crate/package/mise.toml" "crates/$crate/package/hecaton-plugin.yaml" "$out/"
  echo "packaged $name -> $out"
done
