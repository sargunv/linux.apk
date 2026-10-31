#!/usr/bin/env bash
# A windowed compat run always gets a disposable desktop.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
export INNKEEPER_PROFILE=${INNKEEPER_PROFILE:-compat}
if [[ ${INNKEEPER_NESTED:-} != 1 ]]; then
  for arg in "$@"; do
    if [[ $arg == --window ]]; then exec "$repo/tests/nested.sh" "$0" "$@"; fi
  done
fi
cd "$repo"
for arg in "$@"; do
  case $arg in fetch) exec cargo run -q --release -p innkeeper-compat -- "$@" ;; esac
done
# Keep compat sound off the desktop and give audio assertions a session-only
# monitor. An externally started session must use this same sink.
export INNKEEPER_AUDIO_SINK=${INNKEEPER_AUDIO_SINK:-innkeeper_compat_$INNKEEPER_PROFILE}
if ! pactl get-sink-volume "$INNKEEPER_AUDIO_SINK" >/dev/null 2>&1; then
  mod=$(pactl load-module module-null-sink sink_name="$INNKEEPER_AUDIO_SINK" rate=48000 channels=2)
  trap 'pactl unload-module "$mod"' EXIT
fi
cargo run -q --release -p innkeeper-compat -- "$@"
