#!/usr/bin/env bash
# Run Android x86_64 test binaries against our vendor HALs and drivers in a
# fresh named Flatpak session.
#
#   tests/bionic/run.sh [-e VAR=val]... [-v VENDOR_DIR]... [-t TEST_DIR] -- PROG [ARGS...]
#
# -v layers VENDOR_DIR over /vendor through a session addon (first on top),
# -t copies TEST_DIR to /data/nativetest64, and -e passes VAR=val to PROG.
# INNKEEPER_PROPS="name=value;..." sets those properties; read-only ones are
# per-process, through the props shim (props-shim.c) preloaded into PROG.
# TIMEOUT=<s> (default 300) bounds PROG. The composer's frame dumps land in
# out/test/bionic/<profile>/frames.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
session=$repo/tools/dev/session.sh
export INNKEEPER_PROFILE=${INNKEEPER_PROFILE:-bionic}
root=$repo/out/test/bionic/$INNKEEPER_PROFILE
export INNKEEPER_DATA=${INNKEEPER_DATA:-$root/data}
mkdir -p "$root"
envs=() vendor=() testdir=""
while (($#)); do
  case $1 in
    -e)
      envs+=("$2")
      shift 2
      ;;
    -v)
      vendor+=("$(realpath "$2")")
      shift 2
      ;;
    -t)
      testdir=$(realpath "$2")
      shift 2
      ;;
    --)
      shift
      break
      ;;
    *) break ;;
  esac
done
(($#)) || {
  echo 'missing test program' >&2
  exit 2
}
data_home=$("$session" paths | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["data_home"])')
addon=$data_home/innkeeper/addons/bionic
cleanup() {
  "$session" stop >/dev/null 2>&1 || true
  "$session" reset >/dev/null 2>&1 || true
  rm -rf "$addon"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
cleanup
mkdir -p "$addon/vendor"
for ((i = ${#vendor[@]} - 1; i >= 0; i--)); do cp -a "${vendor[i]}/." "$addon/vendor/"; done
for e in "${envs[@]}"; do
  if [[ $e == INNKEEPER_PROPS=* ]]; then
    install -D -m 644 "$repo/out/build/android/tests/bionic/libprops_shim.so" \
      "$addon/vendor/lib64/libinnkeeper_test_props.so"
    break
  fi
done
"$session" start --headless
for _ in $(seq 150); do
  [[ $("$session" exec getprop sys.boot_completed 2>/dev/null) == 1 ]] && break
  sleep 2
done
[[ $("$session" exec getprop sys.boot_completed) == 1 ]] || {
  echo 'session failed to boot' >&2
  exit 1
}
# HAL clients need exclusive access to the composer. Keep init, logd,
# properties and the HALs, but stop framework clients.
"$session" exec stop
for _ in $(seq 100); do
  if ! "$session" exec pidof surfaceflinger >/dev/null; then break; fi
  sleep 0.1
done
if [[ -n $testdir ]]; then
  "$session" exec mkdir -p /data/nativetest64
  tar -C "$testdir" -cf - . | "$session" exec tar -xf - -C /data/nativetest64
fi
args=()
for e in "${envs[@]}"; do
  if [[ $e == INNKEEPER_PROPS=* ]]; then
    IFS=';' read -ra props <<<"${e#*=}"
    for p in "${props[@]}"; do
      if [[ $p != ro.* ]]; then "$session" exec setprop "${p%%=*}" "${p#*=}"; fi
    done
    # Read-only test properties must be per-process; init has already seeded
    # the real property area. Preload from a path permitted by this test's
    # vendor or platform linker namespace.
    args+=("$e")
    if [[ $1 == /vendor/* || $1 == /odm/* ]]; then
      args+=(LD_PRELOAD=/vendor/lib64/libinnkeeper_test_props.so)
    else
      shim=$repo/out/build/android/tests/bionic/libprops_shim.so
      "$session" exec mkdir -p /data/nativetest64
      "$session" exec sh -c 'cat > /data/nativetest64/libprops_shim.so' <"$shim"
      args+=(LD_PRELOAD=/data/nativetest64/libprops_shim.so)
    fi
  else
    args+=("$e")
  fi
done
"$session" exec mkdir -p /data/vendor/innkeeper/frames
rc=0
timeout "${TIMEOUT:-300}" "$session" exec env "${args[@]}" "$@" || rc=$?
mkdir -p "$root/frames"
"$session" exec tar -cf - -C /data/vendor/innkeeper/frames . | tar -xf - -C "$root/frames"
exit "$rc"
