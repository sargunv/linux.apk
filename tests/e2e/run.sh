#!/usr/bin/env bash
# One runner, one private desktop/session, independently selectable bounded groups.
set -Eeuo pipefail
# Report only top-level failures: ERR also fires inside command substitutions.
# shellcheck disable=SC2329 # invoked by the ERR trap
report_error() {
  if ((BASH_SUBSHELL == 0)); then
    echo "e2e: command failed (status=$1) at line $2: $3" >&2
  fi
}
trap 'report_error "$?" "$LINENO" "$BASH_COMMAND"' ERR
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
export INNKEEPER_PROFILE=${INNKEEPER_PROFILE:-e2e}
export INNKEEPER_E2E=1
# The nested desktop is the isolation: the session is a production one, and its
# desktop requests reach desktop.py's fake portals and notifications.
unset INNKEEPER_TEST
S=$repo/tools/dev/session.sh
T=$repo/out/test/e2e/$INNKEEPER_PROFILE
desktop_log=$repo/out/test/nested/$INNKEEPER_PROFILE/desktop.log
bridge_log=$repo/out/test/nested/$INNKEEPER_PROFILE/state/innkeeper-$INNKEEPER_PROFILE/bridge.log
apk=$repo/out/test/probes/InnkeeperProbes.apk
# GitHub's software-rendered desktop is slower than local development.
deadline=55
if [[ ${GITHUB_ACTIONS:-} == true ]]; then
  deadline=${INNKEEPER_E2E_DEADLINE:-55}
fi
observation_wait=$(((20 * deadline + 54) / 55))
timed() (
  local name=$1 start
  shift
  start=$(date +%s%N)
  # shellcheck disable=SC2329 # invoked by the EXIT trap
  finish() {
    local result=$? elapsed
    elapsed=$(($(date +%s%N) - start))
    printf 'e2e: %s elapsed=%d.%03ds status=%d\n' "$name" \
      "$((elapsed / 1000000000))" "$(((elapsed / 1000000) % 1000))" "$result" |
      tee -a "$T/timings.log"
  }
  trap finish EXIT
  trap 'exit 130' INT TERM
  printf '=== e2e: %s\n' "$name" | tee -a "$T/timings.log"
  "$@"
)
s() { "$S" exec "$@"; }
wm() { s am broadcast -n dev.sargunv.innkeeper.system/.wm.ControlReceiver -a "dev.sargunv.innkeeper.$1" "${@:2}"; }
# Poll observations, returning as soon as they arrive; the outer group deadline
# also bounds a command that stops answering.
until_ok() {
  local end=$((SECONDS + ${observation_wait:-20}))
  until "$@"; do
    ((SECONDS < end)) || {
      echo "FAIL waiting for $*" >&2
      return 1
    }
    sleep 0.1
  done
}
booted() { [[ $(s getprop sys.boot_completed 2>/dev/null) == 1 ]]; }
user_unlocked() { [[ $(s am get-started-user-state 0 2>/dev/null) == RUNNING_UNLOCKED ]]; }
log_has() { s logcat -d -s "$1:I" | grep -E "$2" >/dev/null; }
file_has() { grep -E "$2" "$1" >/dev/null; }
window_ready() { s dumpsys activity activities | grep "topResumedActivity=.*$1" >/dev/null; }
launch() {
  local out
  wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
  out=$(wm LAUNCH --es package dev.sargunv.innkeeper.probes --es component "dev.sargunv.innkeeper.probes/.$1")
  display=$(sed -n 's/.*launched display=\([0-9]*\).*/\1/p' <<<"$out")
  [[ -n $display ]] || {
    echo "FAIL launch: $out"
    return 1
  }
  if [[ $1 != DesktopProbe ]]; then until_ok window_ready "$1"; fi
}
stopped() { [[ $("$S" status) == stopped* ]]; }
running() { "$S" status >/dev/null; }
stop_session() {
  "$S" stop >/dev/null
  # The dev supervisor publishes its exit after the sandbox closes. Wait for
  # it before a restart, so its old completion cannot fail the new start.
  if [[ -f $repo/out/dev/flatpak/$INNKEEPER_PROFILE/cgroup ]]; then
    until_ok test -f "$repo/out/dev/flatpak/$INNKEEPER_PROFILE/ended"
  fi
}

if [[ ${1:-} == --group ]]; then
  case $2 in
    boot)
      # Check the real session tree even on hosts with Yama disabled. Memory
      # access on Ubuntu's default ptrace_scope=1 needs an ancestor broker.
      # shellcheck disable=SC2016 # expanded inside Android
      s sh -c '
        for init in $(pidof init); do
          parent=$(sed -n "s/^PPid:[[:space:]]*//p" "/proc/$init/status")
          if test "$(cat /proc/$parent/comm)" = innkeeper-brk; then
            echo "init=$init parent=$parent (innkeeper-brk)"
            exit 0
          fi
        done
        exit 1
      ' >"$T/ancestry.txt"
      cat "$T/ancestry.txt"
      echo 'ok   Android init is a child of the broker'
      "$S" selftest
      # adb: off and authenticated by default; on, a loopback port the user's
      # key reaches, through a private adb server with a throwaway key.
      [[ $(s getprop init.svc.adbd) != running && $(s getprop ro.adb.secure) == 1 ]]
      adb=$ANDROID_HOME/platform-tools/adb
      # The key pair stays in the test's dir; the session sees the public half
      # where it looks for the user's, in the dev sandbox's home.
      rm -rf "$T/adbhome" && mkdir -p "$T/adbhome/.android"
      "$adb" keygen "$T/adbhome/.android/adbkey" >/dev/null 2>&1
      keys=$repo/out/dev/flatpak/$INNKEEPER_PROFILE/home/.android
      mkdir -p "$keys"
      cp "$T/adbhome/.android/adbkey.pub" "$keys/"
      "$repo/tools/dev/innkeeper.sh" profile set --adb on >"$T/adb-on.txt" 2>&1
      serial=$(grep -o '127\.0\.0\.1:[0-9]*' "$T/adb-on.txt")
      port=${serial#*:}
      # shellcheck disable=SC2329 # invoked through until_ok
      listening() { ss -ltnH "sport = :$port" >"$T/adb-listen.txt" && [[ -s $T/adb-listen.txt ]]; }
      until_ok listening
      if grep -v "127.0.0.1:$port" "$T/adb-listen.txt"; then
        echo "FAIL adb listens beyond loopback" >&2
        exit 1
      fi
      # adb reads its key from $HOME/.android.
      A=(env HOME="$T/adbhome" "$adb" -P 15990)
      "${A[@]}" connect "$serial" >/dev/null
      # shellcheck disable=SC2329 # invoked through until_ok
      adb_shell() { [[ $("${A[@]}" -s "$serial" shell echo ok 2>/dev/null) == ok ]]; }
      until_ok adb_shell
      "${A[@]}" kill-server
      "$repo/tools/dev/innkeeper.sh" profile set --adb off >/dev/null 2>&1
      echo "ok   adb is off by default; on, a loopback port the user's key reaches"
      driver=$(s getprop ro.hardware.vulkan)
      feature=android.hardware.vulkan.level=1
      # pm omits =0 when printing a version-zero feature.
      if [[ $driver == lvp ]]; then feature=android.hardware.vulkan.level; fi
      s pm list features >"$T/features.txt"
      file_has "$T/features.txt" "^feature:$feature$"
      echo "ok   Vulkan feature matches $driver: $feature"
      # Session1 reports the same completed boot as the launcher's property check.
      # shellcheck disable=SC2329 # invoked through until_ok
      session_booted() {
        gdbus call --session --dest "dev.sargunv.Innkeeper.Session.p_${INNKEEPER_PROFILE//-/_}" \
          --object-path /dev/sargunv/Innkeeper/Session --method dev.sargunv.Innkeeper.Session1.State >"$T/session-state.txt" &&
          file_has "$T/session-state.txt" '^\(true,'
      }
      until_ok session_booted
      booted
      echo 'ok   Session1 booted follows Android boot completion'
      s cmd package resolve-activity --brief -a android.intent.action.MAIN \
        -c android.intent.category.HOME >"$T/home.txt"
      file_has "$T/home.txt" '^dev.sargunv.innkeeper.system/.wm.HomeActivity$'
      s cmd appops set dev.sargunv.innkeeper.probes MANAGE_EXTERNAL_STORAGE allow
      s am broadcast -n dev.sargunv.innkeeper.probes/.StorageProbe >/dev/null
      until_ok log_has InnkeeperStorageProbe 'state=mounted app-dir=ok MediaStore=ok'
      # A self-finishing activity that handles no configuration changes. If the
      # window manager releases its display while task removal is in flight,
      # Android 17's display-compat recovery relaunches onto the removed display
      # and crashes system_server (#50).
      syspid=$(s pidof system_server)
      am_start() { s am start -n "dev.sargunv.innkeeper.probes/.$1" >/dev/null; }
      am_start TeardownProbe
      until_ok log_has InnkeeperTeardownProbe 'created, display='
      until_ok log_has InnkeeperTeardownProbe 'self-finishing like the storage scan end'
      until_ok log_has InnkeeperTeardownProbe 'destroyed, display='
      # The display is released after close(), so wait until the probe's windows
      # and its "innkeeper:<package>" displays are gone before comparing pids.
      # shellcheck disable=SC2329 # invoked through until_ok
      no_probe_windows() { ! wm LIST | grep -q 'package=dev.sargunv.innkeeper.probes .*window='; }
      # A failed or empty dumpsys must not read as "gone".
      # shellcheck disable=SC2329 # invoked through until_ok
      probe_display_gone() {
        local dump
        dump=$(s dumpsys display 2>/dev/null) || return 1
        [[ $dump == *'Display Devices:'* ]] || return 1
        ! grep -q 'innkeeper:' <<<"$dump"
      }
      until_ok no_probe_windows
      until_ok probe_display_gone
      [[ $(s pidof system_server) == "$syspid" ]] || {
        echo 'FAIL framework restarted while closing a probe window' >&2
        exit 1
      }
      echo 'ok   a self-finishing windowed probe closes without restarting the framework'
      ;;
    input)
      launch InputProbeActivity
      # shellcheck disable=SC2329 # invoked through until_ok
      mapped() { wm LIST | grep "display=$display package=dev.sargunv.innkeeper.probes .*window=yes" >/dev/null; }
      until_ok mapped
      # Position the window for absolute input without resizing Android's
      # display: resize snapshots temporarily block otherwise valid touches.
      if [[ ${INNKEEPER_TEST_COMPOSITOR:-kwin} == mutter ]]; then
        # HWC's first frame can precede Mutter creating its window actor.
        # Retry the compositor operation until it actually found the window.
        # shellcheck disable=SC2329 # invoked through until_ok
        positioned() {
          gdbus call --session --dest dev.sargunv.Innkeeper.TestDesktop --object-path /dev/sargunv/Innkeeper/TestDesktop \
            --method dev.sargunv.Innkeeper.TestDesktop.Position | grep -q true
        }
        until_ok positioned
      else
        cat >"$T/input-window.js" <<'JS'
for (const w of workspace.windowList()) {
  if (String(w.resourceClass).includes("dev.sargunv.innkeeper.probes")) {
    const geometry = w.frameGeometry;
    geometry.x = 300;
    geometry.y = 100;
    w.frameGeometry = geometry;
    workspace.activeWindow = w;
  }
}
JS
        script_id=$(gdbus call --session --dest org.kde.KWin --object-path /Scripting \
          --method org.kde.kwin.Scripting.loadScript "$T/input-window.js" innkeeper-input-window | grep -oE '[0-9]+')
        gdbus call --session --dest org.kde.KWin --object-path "/Scripting/Script$script_id" \
          --method org.kde.kwin.Script.run >/dev/null
        gdbus call --session --dest org.kde.KWin --object-path /Scripting \
          --method org.kde.kwin.Scripting.unloadScript innkeeper-input-window >/dev/null
      fi
      # Activity resume precedes InputDispatcher's focused-window publication.
      # Wait for the actual input target before submitting the burst.
      # shellcheck disable=SC2329 # invoked through until_ok
      input_ready() {
        s dumpsys input >"$T/devices.txt"
        sed -n '/^  FocusedWindows:/,/^  FocusRequests:/p' "$T/devices.txt" |
          grep -E "displayId=$1, name=.*${2:-InputProbeActivity}" >/dev/null &&
          grep -E "name=.*${2:-InputProbeActivity}, id=.*displayId=$1, .*alpha=1," "$T/devices.txt" |
          grep -Ev 'NOT_VISIBLE|NOT_FOCUSABLE' >/dev/null
      }
      until_ok input_ready "$display"
      /usr/bin/python3 "$here/gamepads.py" check "$T/devices.txt"
      # Keep one native seat across the keyboard, click and capture scenarios.
      mkfifo "$T/seat"
      /usr/bin/python3 "$here/seat.py" --stdin <"$T/seat" &
      seat_pid=$!
      exec {seat_fd}>"$T/seat"
      # A submitted first frame precedes the compositor mapping/focusing the surface.
      # Establish real pointer delivery before the uninterrupted test stream.
      # shellcheck disable=SC2329 # invoked through until_ok
      pointer_ready() {
        printf 'abs:800,300\nabs:900,300\n' >&"$seat_fd"
        log_has InnkeeperProbe "generic .*ACTION_HOVER_(ENTER|MOVE).*deviceName=Innkeeper Mouse $display windowDisplay=$display"
      }
      until_ok pointer_ready
      printf '%s\n' key:30,1 key:30,0 button:272,1 button:272,0 >&"$seat_fd"
      until_ok log_has InnkeeperProbe 'key .*KEYCODE_A.*deviceId=-1'
      until_ok log_has InnkeeperProbe "touch .*ACTION_UP.*deviceName=Innkeeper Mouse $display windowDisplay=$display"
      echo 'ok   native keyboard and mouse reach the app'
      launch CaptureProbeActivity
      until_ok mapped
      until_ok input_ready "$display" CaptureProbeActivity
      printf '%s\n' abs:900,400 key:46,1 key:46,0 >&"$seat_fd"
      # One capture assertion: native relative motion reaches the requesting view.
      # shellcheck disable=SC2329 # invoked through until_ok
      captured_motion() {
        printf '%s\n' rel:20,-10 >&"$seat_fd"
        log_has InnkeeperCaptureProbe 'relative source=131076 x=20.0 y=-10.0 device=Innkeeper Mouse'
      }
      until_ok captured_motion
      printf '%s\n' key:19,1 key:19,0 >&"$seat_fd"
      exec {seat_fd}>&-
      wait "$seat_pid"
      rm "$T/seat"
      launch WindowModeProbe
      # A public orientation request reshapes the real host window.
      # shellcheck disable=SC2329 # invoked through until_ok
      shape() {
        wm LIST | /usr/bin/python3 -c '
import re, sys
for line in sys.stdin:
    m = re.search(r"display=" + sys.argv[1] + r" .*size=(\d+)x(\d+)", line)
    if m and ((int(m[1]) < int(m[2])) == (sys.argv[2] == "portrait")):
        sys.exit(0)
sys.exit(1)
' "$display" "$1"
      }
      # Establish the starting shape so an unchanged window cannot pass the request.
      until_ok shape portrait
      s am start --display "$display" -n dev.sargunv.innkeeper.probes/.WindowModeProbe --es action landscape >/dev/null
      until_ok shape landscape
      wm CLOSE --ei display "$display" >/dev/null
      echo 'ok   requested orientation reshapes the host window'
      /usr/bin/python3 "$here/app-settings.py"
      ;;
    audio)
      # The desktop mixer is the only volume control: a stream-volume request
      # must not move STREAM_MUSIC off the maximum index dumpsys reports.
      s cmd media_session volume --stream 3 --set 1 >/dev/null || true
      audio_dump=$(s dumpsys audio)
      grep -q 'mUseFixedVolume=true' <<<"$audio_dump"
      /usr/bin/python3 -c '
import re, sys
dump = sys.argv[1]
block = re.search(r"- STREAM_MUSIC:\n(.*?)(?=\n- STREAM_|\Z)", dump, re.S)
body = block.group(1) if block else ""
max_index = re.search(r"Max: (\d+)", body)
current = re.search(r"Current: (.+)", body)
if (
    not max_index
    or not current
    or not re.search(rf": {max_index[1]}(,|$)", current[1])
):
    print("FAIL fixed volume: STREAM_MUSIC is not at its max index", file=sys.stderr)
    print(block.group(0) if block else "STREAM_MUSIC missing", file=sys.stderr)
    sys.exit(1)
' "$audio_dump"
      echo 'ok   stream volume stays at maximum; the desktop mixer is the control'
      parec -d "$INNKEEPER_AUDIO_SINK.monitor" --format=s16le --rate=48000 --channels=2 --raw >"$T/audio.raw" &
      recorder=$!
      trap 'kill "$recorder" 2>/dev/null || true; wait "$recorder" 2>/dev/null || true' EXIT
      s am start-foreground-service -n "dev.sargunv.innkeeper.probes/.LifecycleProbe\$Media" >/dev/null
      until_ok log_has InnkeeperLifecycleProbe 'foreground audio started'
      # FFT succeeds only after the capture has enough non-silent tone samples.
      until_ok /usr/bin/python3 "$here/audio/freq.py" "$T/audio.raw" --expect 440
      # Android am returns 1 when a service was successfully stopped.
      s am stopservice -n "dev.sargunv.innkeeper.probes/.LifecycleProbe\$Media" >/dev/null || true
      launch BenchProbe
      s am start --display "$display" -n dev.sargunv.innkeeper.probes/.BenchProbe --es mode audio >/dev/null
      s am broadcast -a dev.sargunv.innkeeper.BENCH_BEGIN -p dev.sargunv.innkeeper.probes >/dev/null
      until_ok log_has InnkeeperBench 'BENCH audio .*roundtrip_ns=[1-9][0-9]*'
      echo 'ok   AudioRecord detected the known marker from the private sink monitor'
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      ;;
    desktop)
      launch WideColorProbe
      # A host screenshot checks the displayed pixels, not just Android's buffer.
      # shellcheck disable=SC2329 # invoked through until_ok
      wide_colour_visible() {
        if [[ ${INNKEEPER_TEST_COMPOSITOR:-kwin} == mutter ]]; then
          gdbus call --session --dest dev.sargunv.Innkeeper.TestDesktop --object-path /dev/sargunv/Innkeeper/TestDesktop \
            --method dev.sargunv.Innkeeper.TestDesktop.Screenshot "$T/wide-colour.png" >/dev/null || return 1
        else
          spectacle -b -n -a -o "$T/wide-colour.png" >/dev/null 2>&1 || return 1
        fi
        /usr/bin/python3 "$repo/tests/e2e/wide_colour.py" "$T/wide-colour.png"
      }
      until_ok wide_colour_visible
      echo 'ok   wide-colour window displays its SDR pixels on the host'
      # One live preference round trip, through the same Session1 API as GTK.
      dest=dev.sargunv.Innkeeper.Session.p_${INNKEEPER_PROFILE//-/_}
      gdbus call --session --dest "$dest" --object-path /dev/sargunv/Innkeeper/Session \
        --method dev.sargunv.Innkeeper.Session1.SetPreferences '{"follow_accent":false}' >/dev/null
      until_ok log_has InnkeeperSettings 'desktop settings: .*followAccent=false'
      echo 'ok   preference applied live over Session1'
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      printf '# innkeeper-open-with\n' >"$T/file.md"
      "$repo/tools/dev/innkeeper.sh" open "$T/file.md"
      until_ok log_has InnkeeperOpenWith 'chooser type=text/markdown apps=dev.sargunv.innkeeper.probes'
      until_ok window_ready OpenWithActivity
      chooser=$(wm LIST | sed -n 's/.*display=\([0-9]*\) package=dev.sargunv.innkeeper.system .*/\1/p')
      s input -d "$chooser" keyevent KEYCODE_DPAD_DOWN KEYCODE_ENTER
      until_ok log_has InnkeeperOpenWith 'selected dev.sargunv.innkeeper.probes:'
      until_ok log_has InnkeeperFileProbe 'uri=content://dev.sargunv.innkeeper.system.documents/document/.*type=text/markdown read=true content=# innkeeper-open-with'
      until_ok window_ready FileProbe
      s logcat -d -s InnkeeperOpenWith:I InnkeeperFileProbe:I >"$T/open-with.txt"
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      echo 'ok   Open With opens the selected app with a readable file grant'
      "$here/share.sh"
      s am start -n dev.sargunv.innkeeper.probes/.HandoffProbe --es action view >/dev/null
      until_ok file_has "$desktop_log" '^openfile: shared\.odt$'
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      s am start -a android.intent.action.VIEW -d mailto:production-e2e@example.com >/dev/null
      until_ok file_has "$desktop_log" '^openuri: mailto:production-e2e@example.com$'
      s am start -n dev.sargunv.innkeeper.probes/.NotificationProbe >/dev/null
      until_ok file_has "$desktop_log" '^notification: Ada Lovelace *: Are we still on for the demo at 3\?'
      # No window is needed to exercise the picker result on Android's default display.
      s am start -n dev.sargunv.innkeeper.probes/.DesktopProbe >/dev/null
      until_ok file_has "$desktop_log" '^picker: Open File$'
      until_ok log_has InnkeeperDesktopProbe 'picked: innkeeper-e2e-file'
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      echo 'ok   handoff, VIEW, notification and PICK reached the private desktop fakes'
      ;;
    lifecycle)
      launch BenchProbe
      until_ok log_has InnkeeperBench "BENCH resumed display=$display"
      # shellcheck disable=SC2329 # invoked through until_ok
      bench_mapped() { wm LIST | grep -q "display=$display package=dev.sargunv.innkeeper.probes .*window=yes"; }
      until_ok bench_mapped
      resumes() { s logcat -d -s InnkeeperBench:I | grep -c "BENCH resumed display=$display"; }
      before=$(resumes)
      until_ok /usr/bin/python3 "$here/windows.py" minimize
      until_ok log_has InnkeeperBench "BENCH stopped display=$display"
      # A stop followed by an unintended resume while minimized is a failure.
      [[ $(resumes) -eq $before ]]
      before=$(resumes)
      until_ok /usr/bin/python3 "$here/windows.py" restore
      # shellcheck disable=SC2329 # invoked through until_ok
      resumed_again() { [[ $(resumes) -gt $before ]]; }
      until_ok resumed_again
      echo 'ok   minimizing stops the activity and restoring resumes the same display'
      launch WindowProbe
      s am broadcast -a dev.sargunv.innkeeper.WINDOW_PROBE -p dev.sargunv.innkeeper.probes --es mode flag --ez enabled true >/dev/null
      until_ok file_has "$bridge_log" "window $display idle-inhibit on"
      s am broadcast -a dev.sargunv.innkeeper.WINDOW_PROBE -p dev.sargunv.innkeeper.probes --es mode flag --ez enabled false >/dev/null
      until_ok file_has "$bridge_log" "window $display idle-inhibit off"
      echo 'ok   a visible keep-screen-on request inhibits desktop idle until cleared'
      launch TaskWindowProbe
      first=$display
      # shellcheck disable=SC2329 # invoked through until_ok
      first_task_mapped() { wm LIST | grep -q "display=$first package=dev.sargunv.innkeeper.probes .*window=yes"; }
      until_ok first_task_mapped
      # The sole multiple-task scenario: an app's New Window starts another task on
      # the caller's display. Both tasks must get real toplevels; closing one must
      # leave the other task and window alive.
      s am start --display "$first" --activity-single-top \
        -n dev.sargunv.innkeeper.probes/.TaskWindowProbe --ez new-window true >/dev/null
      # shellcheck disable=SC2329 # invoked through until_ok
      two_task_windows() {
        wm LIST >"$T/task-windows.txt"
        [[ $(grep -c 'package=dev.sargunv.innkeeper.probes .*window=yes' "$T/task-windows.txt") == 2 ]]
      }
      until_ok two_task_windows
      second=$(sed -n 's/.*display=\([0-9]*\) package=dev.sargunv.innkeeper.probes .*/\1/p' "$T/task-windows.txt" | grep -vx "$first")
      wm CLOSE --ei display "$first" >/dev/null
      # shellcheck disable=SC2329 # invoked through until_ok
      second_task_survives() {
        wm LIST >"$T/task-survivor.txt"
        ! grep -q "display=$first package=" "$T/task-survivor.txt" &&
          grep -Eq "display=$second package=dev.sargunv.innkeeper.probes .*tasks=[0-9]+ window=yes" "$T/task-survivor.txt"
      }
      until_ok second_task_survives
      wm CLOSE --ei display "$second" >/dev/null
      echo 'ok   a second task has its own toplevel and survives closing the first'
      s cmd appops set dev.sargunv.innkeeper.probes SCHEDULE_EXACT_ALARM allow
      # Allow four seconds to observe idle stop before the six-second wake lead.
      s am start -n dev.sargunv.innkeeper.probes/.LifecycleProbe --es action alarm --ei seconds 10 >/dev/null
      until_ok log_has InnkeeperLifecycleProbe 'alarm scheduled at='
      wm CLOSE --es package dev.sargunv.innkeeper.probes >/dev/null
      s touch /data/local/tmp/e2e-idle
      until_ok stopped
      echo 'ok   session stopped idle with an alarm pending'
      until_ok running
      until_ok booted
      ;;
  esac
  echo "ok   $2"
  exit
fi

groups=("$@")
((${#groups[@]})) || groups=(boot input audio desktop lifecycle)
for group in "${groups[@]}"; do
  case $group in boot | input | audio | desktop | lifecycle) ;; *)
    echo "unknown e2e group: $group" >&2
    exit 2
    ;;
  esac
done
if [[ ${INNKEEPER_NESTED:-} != 1 ]]; then
  start=$(date +%s%N)
  result=0
  echo "e2e: deadline=${deadline}s (plus at most 4s cleanup)"
  timeout --kill-after=4 "$deadline" "$repo/tests/nested.sh" "$0" "${groups[@]}" &
  runner=$!
  trap 'kill -TERM "$runner" 2>/dev/null || true; wait "$runner" 2>/dev/null || true' INT TERM
  wait "$runner" || result=$?
  trap - INT TERM
  end=$(date +%s%N)
  /usr/bin/python3 -c 'import sys; print(f"e2e: TOTAL wall={(int(sys.argv[2])-int(sys.argv[1]))/1e9:.2f}s")' "$start" "$end"
  exit "$result"
fi
mkdir -p "$T"
: >"$T/timings.log"
export INNKEEPER_IDLE_GRACE=1 INNKEEPER_TEST_TIMERS=1 INNKEEPER_AUDIO_SINK=innkeeper_$INNKEEPER_PROFILE
export INNKEEPER_AUDIO_SOURCE=$INNKEEPER_AUDIO_SINK.monitor
keeper=""
events=""
setup_lock=""
gamepads=""
mod=""
cleanup() {
  local result=$?
  if [[ -n $setup_lock ]]; then exec {setup_lock}>&-; fi
  if ((result != 0)); then
    for log in "$desktop_log" "$repo/out/dev/flatpak/$INNKEEPER_PROFILE/runner.log" \
      "$repo/out/dev/flatpak/$INNKEEPER_PROFILE/supervisor.log"; do
      if [[ -f $log ]]; then
        echo "e2e: last 60 lines of $log:" >&2
        tail -n 60 "$log" >&2
      fi
    done
    echo "e2e: Android failure diagnostics: $T/{crash,system}.txt" >&2
    timeout 1 "$S" exec logcat -d -b crash >"$T/crash.txt" 2>&1 || true
    if file_has "$T/crash.txt" 'FATAL EXCEPTION IN SYSTEM PROCESS'; then
      echo 'e2e: system_server crashed (not a boot readiness timeout):' >&2
      cat "$T/crash.txt" >&2
    fi
    # Keep boot's original error as well as a later recovery/watchdog failure.
    timeout 1 "$S" exec logcat -d -b main,system -t 10000 >"$T/system.txt" 2>&1 || true
  fi
  "$S" stop >/dev/null 2>&1 || true
  "$S" reset >/dev/null 2>&1 || true
  if [[ -n $events ]]; then wait "$events" 2>/dev/null || true; fi
  if [[ -n $keeper ]]; then wait "$keeper" 2>/dev/null || true; fi
  if [[ -n $gamepads ]]; then
    kill "$gamepads" 2>/dev/null || true
    wait "$gamepads" 2>/dev/null || true
  fi
  if [[ -n $mod ]]; then pactl unload-module "$mod"; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM
export INNKEEPER_GAMEPAD_TEST_DIR=$repo/out/dev/flatpak/$INNKEEPER_PROFILE/gp
/usr/bin/python3 "$here/gamepads.py" serve "$INNKEEPER_GAMEPAD_TEST_DIR" &
gamepads=$!
until_ok test -f "$INNKEEPER_GAMEPAD_TEST_DIR/ready"
mod=$(pactl load-module module-null-sink sink_name="$INNKEEPER_AUDIO_SINK" rate=48000 channels=2)
timed setup-stop stop_session
timed setup-reset "$S" reset
mkdir -p "$(dirname "$bridge_log")"
: >"$bridge_log"
timed setup-desktop until_ok file_has "$desktop_log" ready
timed setup-start "$S" start
# The exec service is not ready immediately after start. Hold its existing
# busy lock from the host until a booted Android can run the keeper instead.
exec {setup_lock}>"$XDG_RUNTIME_DIR/app/dev.sargunv.Innkeeper/innkeeper-$INNKEEPER_PROFILE/exec.lock"
flock --shared "$setup_lock"
timed setup-boot until_ok booted
# BOOT_COMPLETED can precede CE app-data preparation on the software runner.
# Installing during unlock can leave a successful install without its data dirs.
timed setup-user-unlock until_ok user_unlocked
: >"$T/keeper.log"
# Do not let the keeper inherit the host lock: lifecycle must stop the session
# after it releases its own exec lock by observing e2e-idle.
s sh -c 'echo ready; while ! test -f /data/local/tmp/e2e-idle; do sleep 0.1; done' \
  {setup_lock}>&- >"$T/keeper.log" 2>&1 &
keeper=$!
timed setup-keeper until_ok file_has "$T/keeper.log" '^ready$'
exec {setup_lock}>&-
setup_lock=""
size=$(stat -c %s "$apk")
timed setup-probes s cmd package install -r -g -S "$size" <"$apk"
# Keep the original process-death events if Android restarts and its noisy boot
# replaces the log ring before a failed group's snapshot. Tags, time and bytes
# are bounded; session stop closes this exec before cleanup waits for it.
(
  # shellcheck disable=SC2016 # expanded by the Android shell
  timeout "$deadline" "$S" exec sh -c '
    logcat -b main,system,crash -T 1 -v threadtime \
      -s Zygote:I AndroidRuntime:E ActivityManager:I ActivityTaskManager:I vold:I Process:I &
    logger=$!
    while ! test -f /data/local/tmp/e2e-events-stop; do sleep 0.1; done
    kill "$logger"
    wait "$logger" 2>/dev/null || true
  ' |
    head -c 2097152 >"$T/session-events.txt"
) &
events=$!
failed=()
run_group() {
  local result=0
  timed "$1" timeout --kill-after=2 "$(((50 * deadline + 54) / 55))" "$0" --group "$1" || result=$?
  if ((result != 0)); then
    # Preserve the failed activity sequence before lifecycle restarts logd.
    tail -n 200 "$bridge_log" >"$T/$1-bridge.log" 2>&1 || true
    timeout 1 "$S" exec logcat -d -t 1000 -s ActivityTaskManager:I ActivityManager:I InnkeeperHandoff:V >"$T/$1-activities.txt" 2>&1 || true
    timeout 1 "$S" exec logcat -d -b main,system,crash -t 10000 >"$T/$1-system.txt" 2>&1 || true
    if [[ -f $repo/out/dev/flatpak/$INNKEEPER_PROFILE/cgroup ]]; then
      local cg stat
      cg=$(cat "$repo/out/dev/flatpak/$INNKEEPER_PROFILE/cgroup")
      for stat in memory.events memory.peak memory.max; do
        if [[ -r $cg/$stat ]]; then
          printf '%s\n' "$stat"
          cat "$cg/$stat"
        fi
      done >"$T/$1-memory.txt"
    fi
  fi
  return "$result"
}
# AudioRecord needs a visible activity. Run it after the window/input checks,
# before lifecycle stops the session; it shares no foreground window with them.
for group in "${groups[@]}"; do
  case $group in audio | lifecycle) continue ;; esac
  run_group "$group" || failed+=("$group")
done
for group in "${groups[@]}"; do
  if [[ $group == audio ]]; then run_group audio || failed+=(audio); fi
done
for group in "${groups[@]}"; do
  if [[ $group == lifecycle ]]; then
    # Logging is an exec too: release its busy lock before testing idle stop.
    s touch /data/local/tmp/e2e-events-stop
    wait "$events" || true
    events=""
    run_group lifecycle || failed+=(lifecycle)
  fi
done
((${#failed[@]} == 0)) || {
  echo "e2e: FAIL (${failed[*]})"
  exit 1
}
echo 'e2e: PASS'
