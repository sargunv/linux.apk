# Innkeeper

Run Android apps on Linux. Each app gets its own Wayland window and follows the
desktop's dark mode, notifications, file pickers, clipboard and media controls.
There's no VM, no visible Android device, and no root, ever.

Innkeeper runs a real Android 17 userspace in a rootless, on-demand session
inside its Flatpak sandbox. The Android image is Google's own AOSP build from
Android's public CI, downloaded and checksummed on your machine. Innkeeper plugs
in at Android's stable vendor interfaces (display, GPU, audio, input) and
bridges them to your desktop.

| Wikipedia                                                                                                                     | F-Droid                                                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| ![Wikipedia reading an article in Innkeeper](https://github.com/user-attachments/assets/68722463-49e5-49fd-80d8-a7b2c498d612) | ![F-Droid app store in Innkeeper](https://github.com/user-attachments/assets/19d9cd5c-673b-4584-9955-f9d169c54822)       |
| **Mindustry**                                                                                                                 | **Shattered Pixel Dungeon**                                                                                              |
| ![Mindustry in Innkeeper](https://github.com/user-attachments/assets/c52f2c4b-0a3f-45bb-a521-0fa0300f545a)                    | ![Shattered Pixel Dungeon in Innkeeper](https://github.com/user-attachments/assets/2f450098-1226-45e9-91e2-407423303333) |
| **Tomato** (Jetpack Compose)                                                                                                  | **OpenCalc**                                                                                                             |
| ![Tomato (Jetpack Compose) in Innkeeper](https://github.com/user-attachments/assets/3229708b-5d53-4cc7-a8df-d096d7d5812f)     | ![OpenCalc in Innkeeper](https://github.com/user-attachments/assets/4ff9dca7-6851-4285-93ca-1e3c2250c66d)                |

## Install

You need an x86_64 Linux desktop on Wayland with a GPU, Linux 6.5 or newer, and
Flatpak 1.15.6 or newer with the Flathub remote. Download
`dev.sargunv.Innkeeper-snapshot.flatpak` from the
[snapshot release](https://github.com/sargunv/innkeeper/releases/tag/snapshot)
and install it:

```sh
flatpak install --user ./dev.sargunv.Innkeeper-snapshot.flatpak
```

There are no automatic updates yet: install a newer file with `--reinstall`.

## Use it

Open an APK with **Innkeeper**: it shows what the app is and which permissions
it wants, installs it, and adds it to your app menu. Each app then opens in its
own window. The first use downloads the Android image (about 1.2 GB).

The same works from a terminal:

```sh
flatpak run dev.sargunv.Innkeeper install App.apk   # also .apks, .xapk, .apkm
flatpak run dev.sargunv.Innkeeper run org.wikipedia
flatpak run dev.sargunv.Innkeeper list
flatpak run dev.sargunv.Innkeeper uninstall org.wikipedia
```

Android starts when an app needs it and stops a couple of minutes after its last
window closes. Profiles keep separate sets of apps and data, and a profile can
show a whole phone or tablet screen instead of one window per app: see the
Profiles page in Innkeeper, or
`flatpak run dev.sargunv.Innkeeper profile --help`.

Apps share one uid and are not isolated from each other, just as native desktop
apps aren't ([security](docs/security.md#single-uid-mode)).

## Status

**Pre-alpha.** Android boots in about 3 s warm. Apps built with Compose,
Flutter, React Native, Capacitor and Godot work, each in its own window with
sound. Not supported:

- apps that need Play Integrity (banking and the like);
- VPN apps (the sandbox has no TUN device) and per-app firewalls or data usage;
- ARM-only apps, which includes most games, until ARM translation lands
  ([milestones](https://github.com/sargunv/innkeeper/milestones)).

## Develop

Innkeeper builds with [mise](https://mise.jdx.dev), the Android NDK, podman and
clang:

```sh
mise install       # toolchains and the pre-commit hook
mise run build     # everything, staged into out/stage
mise run test      # unit tests
```

Start with [docs/architecture.md](docs/architecture.md), which maps every
directory, and [docs/development.md](docs/development.md) for prerequisites,
sessions and tests. Contributor and agent rules are in [AGENTS.md](AGENTS.md).

## License

AGPL-3.0-only.
