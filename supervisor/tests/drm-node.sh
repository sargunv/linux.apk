#!/usr/bin/env bash
# Fake DRM nodes in a private mount namespace; never open a host GPU.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$here/../../out/test/supervisor/drm-node
mkdir -p "$out/dev/dri"
: >"$out/dev/dri/renderD128"
: >"$out/dev/dri/renderD129"
cargo build -q -p innkeeper-broker --example supervisor-probes
cc -O1 -Wall -Werror -o "$out/t" "$here/drm-node.c" "$here/../../out/cargo/debug/examples/libsupervisor_probes.a" -lpthread -ldl -lm
for node in /dev/dri/renderD129 ""; do
  # shellcheck disable=SC2016 # expanded by the inner shell
  INNKEEPER_RENDER_NODE="$node" \
    unshare -Urm sh -c 'mount --bind "$1" /dev; exec "$2"' sh "$out/dev" "$out/t"
done
echo 'ok   DRM dispatch permits only the selected node, none for lavapipe; binder still opens'
