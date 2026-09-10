#!/usr/bin/env bash
# Assembles each named plugin as a directory source under
# $CARGO_TARGET_DIR/plugins/ (target/plugins/ by default; plugins spec
# §17.6): the project's package/ files plus the freshly built binary in
# bin/. The e2e locates the built binary and this output directory the
# same way, relative to $CARGO_TARGET_DIR, so both must agree.
#
# Plugins are standalone projects (Spec H), so the binary is built inside
# plugins/<name>/ and copied here; only the output location is shared.
# With no arguments, every in-tree plugin.
set -euo pipefail
cd "$(dirname "$0")/.."

names=("$@")
(( ${#names[@]} )) || names=(flow web matrix)

out_root="${CARGO_TARGET_DIR:-target}"

for name in "${names[@]}"; do
  crate="hecaton-plugin-$name"
  out="$out_root/plugins/$name"
  if [ -d "plugins/$name" ]; then
    scripts/plugin.sh build "$name"
    built="$(scripts/plugin.sh target-dir "$name")/debug/$crate"
    pkg="plugins/$name/package"
  else
    # Still a core workspace member: Spec H moves the plugins one commit at
    # a time. This branch goes away with the last of them.
    cargo build -q -p "$crate"
    built="$out_root/debug/$crate"
    pkg="crates/$crate/package"
  fi
  rm -rf "$out"
  mkdir -p "$out/bin"
  cp "$built" "$out/bin/$crate"
  cp "$pkg/mise.toml" "$pkg/hecaton-plugin.yaml" "$out/"
  echo "packaged $name -> $out"
done
