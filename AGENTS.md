# Innkeeper

Run Android and OpenHarmony apps on Linux: a real AOSP 17 userspace in a Flatpak
session, with our own vendor HALs, a seccomp supervisor and a desktop bridge.
Start with [docs/architecture.md](docs/architecture.md); its module table says
what each directory is and where it runs.

## Commands

- `mise install`: toolchains and the pre-commit hook.
  `tools/build/toolchain.env` pins the NDK, API and build-tools.
- `mise run check` / `mise run fix`: lint and format everything (hk, dprint).
  The pre-commit hook runs hk on staged files; fix lints, never bypass it.
- `mise run build`: every `build:<module>`, then `stage` into `out/stage`.
  `mise run clean` removes build output.
- `mise run session -- start|stop|status|log|exec|shell|selftest|prepare`.
- `mise run test` (unit), `test:e2e` (grouped runtime checks), `test:bionic`,
  `test:compat`; see [tests/README.md](tests/README.md).

## Rules

- **Never sudo.** Nothing the project needs to build or run may require root, a
  system package, an SELinux module, an AppArmor profile or a kernel module.
  Build dependencies go in the build container or mise. The one exception is
  disposable GitHub-hosted CI runners: `tools/ci/setup.sh` may provision their
  packages, kernel devices and namespace settings with sudo, and refuses to run
  outside GitHub Actions. Contributors' machines get no exception.
- **Memory caps** (a global OOM once crashed the user's desktop). `/tmp` is a
  RAM-backed tmpfs: never write large files there; use `out/`. Development and
  test sessions run capped from the host (`MemoryHigh=6G`, `MemoryMax=8G`),
  container builds always pass `--memory` and `--pids-limit`
  (`tools/container/run.sh`), and long jobs run under `systemd-run --user` with
  a `MemoryMax`. Check `free -g` before starting another session.
- **Never `logcat -b all`** or an unbounded `logcat` into a file; bound it with
  `-d`, `-t <n>` or `-s <tags>`.
- **Host safety.** Android's root is the host user. The Flatpak sandbox is the
  boundary: every namespace running Android code masks and locks host sysfs and
  never binds host `/run/user`, `$HOME` or `/dev` wholesale. Android shares the
  host's network, as any `--share=network` app does
  ([docs/security.md](docs/security.md#flatpak-mode)).
  `innkeeper-session selftest` must pass (`mise run test:e2e -- boot`).
- **Test sessions are named.** Run anything with `INNKEEPER_PROFILE=<name>` and
  a fresh data dir, and stop it when done. Don't start, restart or reset the
  user's main session (no profile name), and don't install host integration
  (`mise run install-dev`, desktop entries, MIME handlers) unless the user asks.
- **Test what you touched, in under a minute.** Keep only tests of observable
  behaviour we own. Unit and low-level tests cover branches, edge cases and
  error handling; e2e keeps one common-path scenario per user-visible feature,
  never variants of the same path. A bug fix comes with a test that fails
  without it; no reproduction, no fix. Iterate on `mise run test` plus the
  relevant `mise run test:e2e -- <group>`. The whole local e2e run must take
  under 60 s, waiting for observations, never fixed delays. GitHub Actions has
  its own deadline for its slower software-rendered desktop
  ([tests/README.md](tests/README.md)). The compat corpus is a manual harness
  for one app (`mise run test:compat -- --only <package>`).
- **Nested test desktops are fully isolated** (a nested plasmashell once started
  kded, KDE Connect and Akonadi against the user's real config, and nested
  portals opened dialogs on the real desktop): a private `dbus-run-session`, a
  private `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY` set to the nested socket before
  the bus starts, `DISPLAY` unset, and
  `HOME`/`XDG_{CONFIG,DATA,CACHE,STATE}_HOME` under `out/`. Never start
  plasmashell, kded, Akonadi or KDE Connect against the user's real dirs, and
  kill everything you started. Recipe:
  [docs/development.md](docs/development.md#a-nested-isolated-desktop).
- **Test buses must not reach the user's runtime dir.** A private D-Bus that can
  activate services while `XDG_RUNTIME_DIR` is the real one starts a second
  `xdg-document-portal`, which takes over `/run/user/$UID/doc` and unmounts the
  user's on exit (this once broke `flatpak run` for half an hour). Always pair a
  private bus with a private `XDG_RUNTIME_DIR`, or use a bus that can't activate
  services. Never restart the user's own portal or D-Bus services.
- **Desktop baseline: current major distro releases only.** Support the oldest
  desktop stack in a current release of Debian, Ubuntu, Fedora, openSUSE or
  Arch. As of 2026-10 that is Debian 13: GNOME 48, Plasma 6.3,
  xdg-desktop-portal 1.20. Use portals and freedesktop specs first, a
  desktop-specific path only where a baseline desktop has no standard one, and
  no fallback for anything older than the baseline.
- **Stable boundaries only.** Never patch the image in place; prefer HAL AIDL,
  NDK APIs, rc and overlay files. Any AOSP patch needs a written reason.
- **Vendor binaries link only LL-NDK system libraries** plus what we ship in
  `/vendor/lib64`, built out of tree with the NDK.
- **Languages:** C/C++ for code loaded into Android processes or on C ABIs; Rust
  for standalone components; Kotlin for the system app.
- **Threat model: desktop parity** ([docs/security.md](docs/security.md)). Keep
  simple bounds and correctness checks; don't add guards that only defend
  against deliberately malicious Android processes.

## Workflow

- Every change goes through a PR (the full-send workflow): a branch, proof,
  review, a PR, CI and review bots green, then merge. Logical commits with clear
  messages; `mise run check` passes before every push. Never force-push `main`.
- The repo is public: never commit secrets, image contents or third-party APKs.
  Generated files go in `out/`. Post nothing outside this repo.
- Never commit media. A visual change needs screenshots on its own PR:
  `gh issue comment <pr> --attach <file>#<alt>`, linked from the description.
  README images go on the README media issue (#1) the same way.
  `tools/dev/window-shot.sh` captures just the Innkeeper window.
- Plans and open work are GitHub issues, not docs. Docs say what is true now and
  why; code comments say what the code can't.
