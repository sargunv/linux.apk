# innkeeper-core

The library the CLI, the manager and the bridge share. It has no artifact of its
own; `cargo test -p innkeeper-core` tests it.

- **Operations** (`ops.rs`): inspect, install, uninstall, run and list, with
  progress through a callback so each front end chooses how to show it. Install
  takes two steps (`inspect`, then `install`) so the manager can confirm in
  between.
- **Install:** APKs and bundles (`.apks`, `.xapk`, `.apkm`; `bundle.rs`) are
  read in-process, without the Android SDK. ABI splits are filtered to the
  session's `ro.product.cpu.abilist`, and one PackageInstaller session streams
  every APK through `install-write -`, with no copy inside Android.
- **APK metadata** (`apk/`): the binary `AndroidManifest.xml` and the
  `resources.arsc` values it refers to, parsed per AOSP's `ResourceTypes.h`.
  Labels are picked per locale as Android picks them (same language, same
  script, closest region), so a desktop entry's `Name[xx]` matches what the app
  shows. Only what an install uses is decoded; malformed input is an error,
  never a panic.
- **Icons** come from Android: the system app's `IconDump` runs under
  `app_process` and renders the launcher icon at 48 to 512 px, so adaptive and
  vector icons get the system mask. The APK's own PNGs are the fallback. Before
  installation, `apk/icon.rs` renders static adaptive and vector layers on the
  host with resvg ([manager limits](../manager/README.md#install-page)).
- **Desktop entries** (`desktop.rs`, `xdg.rs`, `dynlauncher.rs`): each app's
  record, `dev.sargunv.innkeeper.<pkg>.desktop`, stays in the Flatpak's own data
  dir, with custom URL schemes as `x-scheme-handler/*` (never `http(s)`, `file`,
  `content` or `intent`). It reaches the app menu through the DynamicLauncher
  portal, after the user confirms its dialog, as
  `dev.sargunv.Innkeeper.<pkg>.desktop` running
  `flatpak run --command=innkeeper` for the entry's profile, which survives
  updates. Windows (`StartupWMClass`), notifications and MPRIS players use that
  id. At manager start, entry repair offers an entry again when it is missing or
  runs another command, unless the user declined it.
- **File types:** the system app reports, through `IconDump`, the concrete MIME
  types of each app's exported, enabled, default VIEW filters that accept
  `content://` files. Innkeeper's own desktop entry registers common document,
  text and media types and forwards files to an **Open with Android app…**
  chooser ([the CLI](../cli/README.md)).
- **Profiles and adb** (`profiles.rs`, `adb.rs`): listing, creating, renaming,
  deleting, starting and stopping profiles, and connecting the user's adb server
  to one over its own protocol on loopback.
- **Per-app settings** (`app_settings.rs`) and **preferences**
  (`preferences.rs`) go through the bridge's session D-Bus API, or are saved for
  the next start when no session runs.
- **Paths** (`paths.rs`): the prefix relative to the running executable and the
  XDG dirs. A profile's own paths and settings come from the launcher's library
  ([host/launcher](../launcher/README.md)).
- **Session** (`launcher.rs`): starts the session on demand and waits for boot
  and user unlock. Whether it runs and whether the image is ready come from the
  launcher's library, in-process; start, stop and `exec` run
  `innkeeper-session`, whose `exec` must join the session from a single-threaded
  process. Commands inside Android are argv, never shell strings.
- **Errors** (`errors.rs`): `ErrorKind` context on anyhow errors lets front ends
  pick user-facing copy without parsing diagnostics. PackageManager install
  codes are parsed once here, unknown future codes included.
- **Untrusted input** (`validate.rs`): package names, labels, schemes, locales
  and URIs are validated before they reach a path, an entry or a command.

## Known limits

- The DynamicLauncher portal in xdg-desktop-portal 1.20 rejects
  `[Desktop Action]` groups and has no silent update, so app-menu entries have
  no actions, and a repaired entry asks again through the portal's dialog.
