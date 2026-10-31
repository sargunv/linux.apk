#!/usr/bin/env bash
# Run a command on a disposable Wayland desktop with a private bus and XDG dirs.
set -Eeuo pipefail
trap 'echo "nested: command failed (status=$?) at line $LINENO: $BASH_COMMAND" >&2' ERR
repo=$(cd "$(dirname "$0")/.." && pwd)
export INNKEEPER_IMAGES=${INNKEEPER_IMAGES:-${XDG_DATA_HOME:-$HOME/.local/share}/innkeeper/images}
export INNKEEPER_PROFILE=${INNKEEPER_PROFILE:-nested}
export INNKEEPER_TEST_COMPOSITOR=${INNKEEPER_TEST_COMPOSITOR:-kwin}
N=$repo/out/test/nested/$INNKEEPER_PROFILE
host_runtime=$XDG_RUNTIME_DIR
rt=$(mktemp -d -p "$host_runtime" inkp-XXXXXX)
unit=inkp-nested-$INNKEEPER_PROFILE
phase=bootstrap
private_bus=""
cleanup() {
  local result=$? portal
  # Cleanup failures must not replace the nested command's exit status.
  trap - ERR
  set +e
  journalctl --user -u "$unit" --no-pager -n 80 >"$N/service.log" 2>&1 || true
  if ((result != 0)); then
    echo "nested: $phase failed (compositor=$INNKEEPER_TEST_COMPOSITOR, status=$result)" >&2
    for log in "$N/desktop.log" "$N/compositor.log" "$N/service.log"; do
      if [[ -f $log ]]; then
        echo "nested: last 80 lines of $log:" >&2
        tail -n 80 "$log" >&2
      fi
    done
  fi
  # A deadline can interrupt the runner before its trap reaches session stop.
  # Flatpak's scope is outside this desktop's service; reap that test scope too.
  if [[ ${INNKEEPER_E2E:-} == 1 && -f $repo/out/dev/flatpak/$INNKEEPER_PROFILE/cgroup ]]; then
    local cg
    cg=$(cat "$repo/out/dev/flatpak/$INNKEEPER_PROFILE/cgroup")
    DBUS_SESSION_BUS_ADDRESS="unix:path=$host_runtime/bus" XDG_RUNTIME_DIR="$host_runtime" \
      systemctl --user stop "${cg##*/}" 2>/dev/null || true
  fi
  # Resolve only the owner on our private bus, before stopping that bus.
  # Looking up an absent owner does not activate a new portal during teardown.
  if [[ -n $private_bus ]]; then
    portal=$(timeout 1 gdbus call --address "$private_bus" --dest org.freedesktop.DBus \
      --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.GetConnectionUnixProcessID \
      org.freedesktop.portal.Documents 2>/dev/null)
    if [[ $portal =~ ^\(uint32\ ([0-9]+),\)$ ]]; then
      kill "${BASH_REMATCH[1]}" 2>/dev/null || true
    fi
  fi
  DBUS_SESSION_BUS_ADDRESS="unix:path=$host_runtime/bus" XDG_RUNTIME_DIR="$host_runtime" \
    systemctl --user stop "$unit" 2>/dev/null || true
  if mountpoint -q "$rt/doc"; then
    timeout 1 fusermount3 -u "$rt/doc" 2>/dev/null ||
      timeout 1 fusermount3 -uz "$rt/doc" 2>/dev/null
  fi
  # Never descend into a portal mount which could not be detached.
  if mountpoint -q "$rt/doc"; then
    echo "nested: cleanup leaked private runtime $rt (document portal still mounted)" >&2
  elif ! rm -rf "$rt"; then
    echo "nested: cleanup leaked private runtime $rt (removal failed)" >&2
  fi
  return "$result"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
mkdir -p "$N"/{home,config,data,cache,state}
: >"$N/compositor.log"
# KWin grants fake seat input and screenshots only to an executable whose
# desktop file names them: tests/e2e/kwin.py. This entry lives solely on this
# disposable desktop.
mkdir -p "$N/data/applications"
cat >"$N/data/applications/dev.sargunv.Innkeeper.TestSeat.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Innkeeper test seat
Exec=$(readlink -f /usr/bin/python3)
NoDisplay=true
X-KDE-Wayland-Interfaces=org_kde_kwin_fake_input
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
DESKTOP

if [[ -n ${INNKEEPER_TEST_LAUNCHER_PORTAL:-} ]]; then
  mkdir -p "$N/config/xdg-desktop-portal"
  printf '[preferred]\norg.freedesktop.impl.portal.DynamicLauncher=%s\n' \
    "$INNKEEPER_TEST_LAUNCHER_PORTAL" >"$N/config/xdg-desktop-portal/portals.conf"
fi
cat >"$N/hold.sh" <<HOLD
#!/bin/sh
echo "\$DBUS_SESSION_BUS_ADDRESS" >"$N/bus"
exec /usr/bin/python3 "$repo/tests/e2e/kwin.py" "$N"
HOLD
chmod +x "$N/hold.sh"
# Explicit daemon config avoids system fragments (including network devices).
mkdir -p "$N/config/pipewire" "$N/config/wireplumber/wireplumber.conf.d"
cat >"$N/config/pipewire/test.conf" <<'AUDIO'
context.properties = { core.daemon = true core.name = pipewire-0 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [
  { name = libpipewire-module-protocol-native }
  { name = libpipewire-module-metadata }
  { name = libpipewire-module-spa-node-factory }
  { name = libpipewire-module-client-node }
  { name = libpipewire-module-access }
  { name = libpipewire-module-adapter }
  { name = libpipewire-module-link-factory }
]
context.objects = [
  { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver node.group = pipewire.dummy priority.driver = 20000 } }
]
AUDIO
cp /usr/share/pipewire/pipewire-pulse.conf "$N/config/pipewire/test-pulse.conf"
# No hardware monitors: test capture must never see a real microphone.
cat >"$N/config/wireplumber/wireplumber.conf.d/90-private-audio.conf" <<'AUDIO'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
AUDIO
cat >"$N/compositor.sh" <<HOLD
#!/usr/bin/env bash
set -Eeuo pipefail
trap 'echo "nested bootstrap: command failed (status=\$?) at line \$LINENO: \$BASH_COMMAND" >&2' ERR
if [ "${INNKEEPER_E2E:-}" = 1 ]; then
  /usr/bin/python3 "$repo/tests/e2e/desktop.py" "$N/data/picked.txt" >"$N/desktop.log" 2>&1 &
  gdbus wait --session --timeout 10 org.freedesktop.portal.Desktop
fi
pipewire -c "$N/config/pipewire/test.conf" >"$N/pipewire.log" 2>&1 &
pipewire-pulse -c "$N/config/pipewire/test-pulse.conf" >"$N/pulse.log" 2>&1 &
wireplumber >"$N/wireplumber.log" 2>&1 &
/usr/libexec/at-spi-bus-launcher --launch-immediately --a11y=1 &
gdbus wait --session --timeout 10 org.a11y.Bus
/usr/libexec/at-spi2-registryd &
if [ "$INNKEEPER_TEST_COMPOSITOR" = mutter ]; then
  echo "\$DBUS_SESSION_BUS_ADDRESS" >"$N/bus"
  exec gnome-shell --headless --wayland --no-x11 --virtual-monitor 3200x1080 \\
    --wayland-display inkp-nested --automation-script "$repo/tests/e2e/mutter.js"
fi
exec kwin_wayland --virtual --no-lockscreen --socket inkp-nested \\
  --width 1920 --height 1080 --exit-with-session "$N/hold.sh"
HOLD
chmod +x "$N/compositor.sh"
printf '%s\n' "$rt" >"$N/runtime"
rm -f "$N/bus"
export PULSE_SERVER=unix:$rt/pulse/native
export FLATPAK_USER_DIR=$repo/out/flatpak/user
export XDG_DATA_DIRS=$FLATPAK_USER_DIR/exports/share:/usr/local/share:/usr/share
export LANG=C.UTF-8 LANGUAGE=C
export HOME=$N/home XDG_CONFIG_HOME=$N/config XDG_DATA_HOME=$N/data
export XDG_CACHE_HOME=$N/cache XDG_STATE_HOME=$N/state
export XDG_CURRENT_DESKTOP=KDE
if [[ $INNKEEPER_TEST_COMPOSITOR == mutter ]]; then export XDG_CURRENT_DESKTOP=GNOME; fi
unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS
# Explicit renderer overrides belong to this desktop, not the user's manager.
renderer_env=()
for key in LIBGL_ALWAYS_SOFTWARE KWIN_COMPOSE; do
  if [[ -v $key ]]; then renderer_env+=("$key=${!key}"); fi
done
# systemd itself is reached through the host bus; everything it starts inherits
# the private runtime and the nested Wayland socket before dbus-run-session.
phase='starting private compositor service'
DBUS_SESSION_BUS_ADDRESS="unix:path=$host_runtime/bus" systemd-run --user --quiet --collect \
  --unit "$unit" -p MemoryMax=3G -p "StandardOutput=append:$N/compositor.log" \
  -p StandardError=inherit -- env -u DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
  PATH="$PATH" LANG="$LANG" LANGUAGE="$LANGUAGE" HOME="$HOME" XDG_RUNTIME_DIR="$rt" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" \
  XDG_DATA_HOME="$XDG_DATA_HOME" XDG_CACHE_HOME="$XDG_CACHE_HOME" XDG_STATE_HOME="$XDG_STATE_HOME" \
  XDG_DATA_DIRS="$XDG_DATA_DIRS" XDG_CURRENT_DESKTOP="$XDG_CURRENT_DESKTOP" FLATPAK_USER_DIR="$FLATPAK_USER_DIR" \
  INNKEEPER_TEST_COMPOSITOR="$INNKEEPER_TEST_COMPOSITOR" \
  PULSE_SERVER="$PULSE_SERVER" WAYLAND_DISPLAY=inkp-nested \
  "${renderer_env[@]}" \
  dbus-run-session -- "$N/compositor.sh"
phase='waiting for private Wayland socket'
for _ in $(seq 100); do
  [[ -s $N/bus && -S $rt/inkp-nested ]] && break
  sleep 0.1
done
[[ -s $N/bus && -S $rt/inkp-nested ]] || {
  echo 'nested desktop did not start' >&2
  exit 1
}
DBUS_SESSION_BUS_ADDRESS=$(cat "$N/bus")
private_bus=$DBUS_SESSION_BUS_ADDRESS
export XDG_RUNTIME_DIR=$rt WAYLAND_DISPLAY=inkp-nested DBUS_SESSION_BUS_ADDRESS
export INNKEEPER_NESTED=1
phase='waiting for the test desktop service dev.sargunv.Innkeeper.TestDesktop'
gdbus wait --session --timeout 10 dev.sargunv.Innkeeper.TestDesktop
phase='waiting for private audio server'
for _ in $(seq 100); do
  if pactl info >/dev/null 2>&1; then break; fi
  sleep 0.05
done
pactl info >"$N/audio-server.txt"
phase='running nested command'
"$@"
