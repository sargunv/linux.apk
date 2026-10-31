# launcher

`innkeeper-session` (crate `innkeeper-session`), staged to `libexec/innkeeper/`
by `mise run build:host`. It runs as the user inside the Innkeeper Flatpak,
builds nothing and finds the stage relative to itself. What the session is and
does is in [docs/session.md](../../docs/session.md); `--help` prints the usage.
Its library gives [host/core](../core/README.md) a profile's paths, settings and
status without running the launcher.

Subcommands: `start [-f] [--headless]`, `stop`, `status` (prints
`running: host pid N` or `stopped`), `prepare`, `image`, `reset`, `log [-f]`,
`exec CMD...`, `shell`, `selftest`. The internal `_serve` command owns the
session in a sandbox of its own.

| Module                              | What                                                                                   |
| ----------------------------------- | -------------------------------------------------------------------------------------- |
| `cli.rs`                            | the commands; start here                                                               |
| `paths.rs`, `conf.rs`               | every path, and `session.conf`: the one config `start` writes and the others read      |
| `profile.rs`, `preferences.rs`      | profile names and settings; the preferences the launcher and desktop clients share     |
| `image.rs`, `prune.rs`, `layer.rs`  | preparing and collecting the image cache; the start-time layer, cmdline and bootconfig |
| `flatpak.rs`, `control.rs`          | the composed root, the sandbox's `/`, the session's lifetime and its control socket    |
| `root.rs`, `init.rs`, `session.rs`  | partitions, binder nodes, `/innkeeper`, pivot-exec, the kmsg reader and the bridge     |
| `graphics.rs`, `phone.rs`, `adb.rs` | the render node and Vulkan driver, phone mode, adb on a loopback port                  |
| `exec.rs`, `selftest.rs`            | `exec` (control socket, then the exec relay) and the host-safety probes                |
| `lifecycle.rs`                      | stopping when idle, and the wakeup for the next alarm                                  |
| `sys.rs`                            | syscall wrappers                                                                       |

| Variable                                              | For                                                             |
| ----------------------------------------------------- | --------------------------------------------------------------- |
| `INNKEEPER_PROFILE`                                   | the profile to run (required)                                   |
| `INNKEEPER_TEST=1`                                    | tests: a test session, isolated from desktop integration        |
| `INNKEEPER_AUDIO_SOURCE`                              | users: Pulse/PipeWire capture source (default: desktop default) |
| `INNKEEPER_AUDIO_SINK`                                | users: one PipeWire sink for the session's audio                |
| `INNKEEPER_IDLE_GRACE`                                | users: seconds idle before the session stops (0: never)         |
| `INNKEEPER_VULKAN=lvp`                                | debugging: software Vulkan (lavapipe) instead of a render node  |
| `--headless`                                          | tests: no windows                                               |
| `INNKEEPER_TEST_TIMERS`                               | tests: short idle polling and alarm wake lead                   |
| `INNKEEPER_DATA`, `INNKEEPER_IMAGES`, `INNKEEPER_PIN` | tests: Android's `/data`, the image cache, the pin              |
| `INNKEEPER_BRIDGE_VERBOSE`, `INNKEEPER_BROKER_DEBUG`  | debugging: bridge stream stats; broker tracing                  |

Tests: `cargo test -p innkeeper-session` (in `mise run test`), and
`mise run test:e2e -- boot`, which runs `selftest` in a fresh named profile.
Development sessions run through `tools/dev/session.sh`, the staged launcher in
a private Flatpak ([packaging/flatpak](../../packaging/flatpak/README.md)).

`session.log` records launcher milestones with elapsed milliseconds and host
`CLOCK_BOOTTIME` timestamps. Compare them with init's `ro.boottime.*` properties
and the event log's `boot_progress_*` timestamps to separate setup time from
Android's boot without reading host `dmesg`.
