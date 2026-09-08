#!/usr/bin/env bash
# Fetches the pinned xterm.js build into crates/hecaton-plugin-web/assets/
# and verifies every file against VENDOR.md (plugins spec §18.5). cargo
# audit and deny.toml do not cover JavaScript; the recorded digests are
# the supply-chain control. To bump: change the versions below, run this,
# copy the printed digests into VENDOR.md, run it again.
set -euo pipefail
cd "$(dirname "$0")/.."

XTERM=6.0.0
FIT=0.11.0
out=crates/hecaton-plugin-web/assets
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL -o "$tmp/xterm.tgz" "https://registry.npmjs.org/@xterm/xterm/-/xterm-$XTERM.tgz"
curl -sSfL -o "$tmp/fit.tgz" "https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-$FIT.tgz"
mkdir -p "$tmp/xterm" "$tmp/fit" "$out"
tar -xzf "$tmp/xterm.tgz" -C "$tmp/xterm" package/lib/xterm.js package/css/xterm.css package/LICENSE
tar -xzf "$tmp/fit.tgz" -C "$tmp/fit" package/lib/addon-fit.js
cp "$tmp/xterm/package/lib/xterm.js" "$out/xterm.js"
cp "$tmp/xterm/package/css/xterm.css" "$out/xterm.css"
cp "$tmp/xterm/package/LICENSE" "$out/LICENSE.xterm"
cp "$tmp/fit/package/lib/addon-fit.js" "$out/addon-fit.js"

echo "tarballs:"
(cd "$tmp" && sha256sum xterm.tgz fit.tgz)
echo "files:"
(cd "$out" && sha256sum xterm.js xterm.css addon-fit.js) | tee "$tmp/sums"
while read -r sum file; do
  grep -q "$sum" "$out/VENDOR.md" || { echo "VENDOR.md does not record $file $sum" >&2; exit 1; }
done < "$tmp/sums"
echo "assets match VENDOR.md"
