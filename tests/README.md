# Tests

Unit and low-level tests carry branches, edge cases and error handling; e2e
keeps one common-path scenario per user-visible feature. Move variants down
before removing them from e2e, preserving every bug regression at the cheapest
level.

- `mise run test`: Rust, Kotlin protocol/input, HAL host tests, supervisor,
  image scripts and harness units. No Android session or APK corpus needed.
- `mise run test:e2e -- [group...]`: one fresh named session on an isolated
  desktop. No arguments runs five groups: `boot` (host safety, HOME and
  storage), `input`, `audio`, `desktop` (including sharing) and `lifecycle`.
  Build and prepare first:

  ```sh
  mise run build
  INNKEEPER_PROFILE=prepare-tests mise run session -- prepare
  INNKEEPER_PROFILE=e2e-local mise run test:e2e
  ```

The runner owns a private desktop: D-Bus, runtime, HOME and XDG dirs, PipeWire
and Pulse servers without hardware monitors, a null audio sink and fake portals
and notifications
([nested desktop rules](../docs/development.md#a-nested-isolated-desktop)). It
stops and resets its profile on exit, also on failure. Check `free -g` first;
sessions are memory capped.

Local e2e has a 55 s deadline plus at most 4 s cleanup, and groups wait for
observations, never fixed delays. Audio runs after the window checks, since
AudioRecord needs a visible activity. Setup waits for user 0's
`RUNNING_UNLOCKED` before installing probes: boot completion can precede
credential-encrypted app-data preparation. CI runs headless Mutter with llvmpipe
(`INNKEEPER_TEST_COMPOSITOR=mutter LIBGL_ALWAYS_SOFTWARE=1`) and a 180 s
deadline (`INNKEEPER_E2E_DEADLINE`); observation and group limits scale with it.
Logs, captures and phase timings land in `out/test/e2e/<profile>/` and desktop
logs in `out/test/nested/<profile>/`; CI uploads both as `checks-logs`.
`desktop` checks real desktop policy through a capped bridge sidecar on the
private desktop, without opening host apps or dialogs.

- `mise run test:bionic`: manual Android-side HAL diagnostics.
- `mise run test:compat -- --only <package>`: manual app troubleshooting; see
  [compat/README.md](compat/README.md). The full corpus runs only in nightly CI.
  Never trigger, run or wait for it during development or PR work.
