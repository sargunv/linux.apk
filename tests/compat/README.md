# compat

A manual app-troubleshooting harness, never a PR or agent loop gate: crate
`innkeeper-compat`, run with `mise run test:compat -- --only <package>,...`
(usage in the header of `src/main.rs`). It installs each selected app of
[corpus.toml](corpus.toml) (pinned F-Droid APKs, fetched to
`out/test/compat/corpus` and never committed) into a session, and writes
`out/test/compat/<profile>/results.json`. The run fails unless every app works.

Per app (`src/app.rs`): `pm install`, launch through the window manager, watch 8
s (15 s for games) for crashes, ANRs and death, check window focus, take a
screenshot that must not be blank (one colour over 99.5%), run the optional
script (syntax at the top of `corpus.toml`), then tap, scroll and Back, and scan
the log for known failure signatures (MediaCodec, WebView, GL/EGL/Vulkan errors,
Play services). The last UI dump is saved as `ui.xml` for script failures. Logs
and screenshots go to `out/test/compat/<profile>/<pkg>/`.

Compat audio goes to a private null sink, `innkeeper_compat_<profile>` (override
with `INNKEEPER_AUDIO_SINK`). The `audio:` script step records up to three
seconds of its monitor and asserts non-silent PCM above -60 dBFS; `audio.raw`
and `audio.txt` keep the capture and level analysis. Run it while the app's
background music is playing, before taps or sound effects. A session started
outside the harness must use the same sink. `pactl` and `parec` must be
available for a run.

- **Results:** works, partly works (the first frame was fine, a later check
  failed), or doesn't work. A session restart during an app is retried once and
  never counts against it.
- **Session:** the named profile `INNKEEPER_PROFILE` (default `compat`) if it is
  running, or else one it starts (headless unless `--window`) and stops at the
  end. A named profile is a test session, so links are only logged. Run it at
  nice 0: uiautomator dies when it can't reset its thread priority, so the
  harness re-runs itself at nice 0 when it finds itself niced.
- **Frameworks** in the corpus were identified by hand from each APK's libraries
  and assets (`libflutter.so`, `libhermes.so`, `assets/capacitor.config.json`,
  `libgodot_android.so`, Compose classes, `libgdx.so`, `libmaplibre.so`); there
  is no FOSS Unity or Xamarin app to test.
- **Triage:** inspect a failing app's failure signature in its
  `out/test/compat/<profile>/<pkg>/` logs; `manual` notes in the corpus list
  checks the harness doesn't automate yet.
- **Flatpak**: there is no TUN device, so Rethink's VPN check fails and it
  partly works. A `--window` run gets a nested desktop of its own
  (`tests/nested.sh`).
