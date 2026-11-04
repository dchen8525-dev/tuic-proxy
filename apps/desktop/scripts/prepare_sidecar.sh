#!/bin/sh
# Stage the built tuic-proxy-client into the Tauri sidecar directory.
#
# Tauri's externalBin expects a target-triple suffix:
#   src-tauri/binaries/tuic-proxy-client-x86_64-unknown-linux-gnu
#
# Usage:
#   sh apps/desktop/scripts/prepare_sidecar.sh target/release [TARGET_TRIPLE]

set -eu

bin_dir=${1:?usage: prepare_sidecar.sh BIN_DIR [TARGET_TRIPLE]}
target=${2:-$(rustc -vV | sed -n 's/^host: //p')}

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
src="$bin_dir/tuic-proxy-client"
if [ ! -f "$src" ]; then
  echo "cannot find $src; run cargo build --release at the repo root first" >&2
  exit 1
fi

dst_dir="$here/src-tauri/binaries"
mkdir -p "$dst_dir"
dst="$dst_dir/tuic-proxy-client-$target"
cp "$src" "$dst"
chmod +x "$dst"
echo "sidecar: $dst"
