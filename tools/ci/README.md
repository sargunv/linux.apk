# GitHub Actions

## CI

`CI` runs on pull requests and pushes to main. It classifies the changed paths
(`changed-paths.sh`), then runs lint and one release build. Unit tests and e2e
run in parallel on separate runners from that build's `stage` artifact (release
tarball, test probes and `manifest.tsv`; tarballs keep modes and symlinks). The
`checks` job is the single required gate: it fails if any applicable job did not
succeed, so a failed dependency cannot turn into a green skip.

A change touching only README files, `AGENTS.md`, Markdown under `docs/` or
`LICENSE` runs formatting and script/workflow lint, without Clippy, build or
runtime tests. Unknown paths, missing history and new branches run everything.
PRs compare the base and merge commits; pushes compare the previous and current
commit.

Inspect a run with `gh run view <run-id>`.

## Release

`Release` runs after a successful CI push to main, on a pushed `v*` tag and on
`workflow_dispatch`. A snapshot packages the commit CI tested from that commit's
stage cache and replaces the floating `snapshot` prerelease, deleting any other
asset on it; a newer push cancels one in progress. A tag attaches
`<app>-stable.flatpak` to its release. A dry run publishes nothing. Details:
[packaging/flatpak](../../packaging/flatpak/README.md#release).

## Runners

Ubuntu 26.04 supplies the GTK baseline, Mutter and Flatpak. Only `setup.sh`,
which refuses to run outside GitHub Actions, uses sudo: it installs each job
mode's packages, enables unprivileged user namespaces, starts the systemd user
manager and PipeWire, and opens udmabuf (built into the runner's Azure kernel)
to the runner user. A missing udmabuf fails setup: software Vulkan still needs
real dma-buf allocations, so a fake device would not test Android graphics.

The GPU-less VM runs headless GNOME Shell/Mutter with surfaceless llvmpipe,
which unlike KWin's virtual backend needs no DRM node and advertises
linux-dmabuf v3. The invocation follows
[GTK's headless CI runner](https://github.com/GNOME/gtk/blob/main/.gitlab-ci/run-tests.sh)
and
[GNOME Shell's automation tests](https://github.com/GNOME/gnome-shell/blob/main/tests/meson.build).
CI selects it with `INNKEEPER_TEST_COMPOSITOR=mutter`; local runs default to
virtual KWin.

The test tasks run the stage through the development Flatpak driver
(`tools/dev/run.py`) on a `tests/nested.sh` desktop, so sessions are named and
memory capped as locally. CI sets `INNKEEPER_MIN_AVAILABLE_GIB=10` for the 16 GB
runner; the local default is 25 GiB, and the 8 GiB hard cap is unchanged.

## Caches

- **Stage:** keyed by the Git tree digest (paths, modes, symlinks, contents) of
  every source, build script, workflow and toolchain input. An exact hit
  restores the stage and probes APK and skips the build job's setup and build;
  tests still run.
- **Cargo, ccache, Gradle:** keyed by OS/architecture, job mode and
  dependency/toolchain pins, with prefix restores; each tool checks its own
  inputs. Mesa/LLVM and the image tools keep their own input hashes. Cold
  Mesa/LLVM builds fit in the 120-minute build timeout. The package job does not
  compile, so it skips these caches: its bundle fills `out/` with more files
  than the compile key's `hashFiles` can walk.
- **Image:** the prepared Android image, keyed by its pin, preparation scripts,
  image tools, container and launcher preparation code. Archives drop xattrs, so
  preparation always recreates the small CLAT layer that carries Android
  ownership xattrs.
- **Private Flatpak:** keyed by the dev driver, `dev-init.sh` and the manifest.
  The dev deployment always runs the current stage through `--app-path`.

SDK, image and Flatpak caches save right after they are prepared, and build and
unit caches at the end of their own job, so a later failure cannot discard them.
Android session data is never cached, and no Android image or third-party APK is
uploaded as an artifact.

To measure incremental compilation, rerun a PR run at its pinned commit after
deleting only its stage cache. New PR events can merge a newer main with changed
pins, so they may legitimately be cold.
