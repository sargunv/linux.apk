# audio

Our AIDL audio module, `IModule/innkeeper`, plays and records through the shared
bridge client and the bridge's PipeWire streams. `StreamBridge` (output) and
`StreamInBridge` (input) sit on our `libtinyalsav2.so`, a tinyalsa PCM backend
that replaces the image's copy. The service, rc and library are an overlay in
`/apex/com.android.hardware.audio/`; the image stays unmodified. The frozen NDK
AIDL bindings are generated at configure time with the pinned build-tools, like
the graphics HAL.

The image's default primary HAL keeps its microphone (address `bottom`, also on
our tinyalsa) and telephony. The policy XML removes its speaker, MMAP and
offload ports; MMAP and offload were silent stubs. Our microphone, with an empty
address, is the default input.

- **One volume control.** Android's stream volumes stay at maximum
  (`config_useFixedVolume`), so an app slider and the desktop mixer don't stack.
  An app's own per-player gain still applies.
- **Why our own output stream.** The stock HAL hardcodes an 85 ms nominal
  latency and a 4096-frame minimum buffer at 48 kHz, and its emulator workaround
  skips transfers when uptime pacing falls behind. Our module offers a 240-frame
  (5 ms) burst and the `FAST` flag, so AudioFlinger can use FastMixer.
- **Why our own input stream.** The stock HAL's MonoPipe writes sleep for 1.15
  times the audio duration, so a 4096-frame request arrived every 98 ms while
  RecordThread consumed every 85 ms, inserting silence; no tinyalsa period or
  ring size changes that request. `StreamInBridge` transfers 480-frame (10 ms)
  bursts straight to the framework FMQ. Selecting the stock microphone still
  hits that limitation.
- **Pacing and timestamps.** Writes block on ring space, which follows
  PipeWire's audio clock; there is no uptime pacer and no skipped transfer.
  Presentation timestamps include the ring and the downstream queue.
- **Buffers.** Playback uses two 240-frame periods (10 ms) and the bridge
  requests another 20 ms downstream, so the output port reports 30 ms nominal
  latency; stream replies report the observed queued frames. `audio.prop` sets
  AudioFlinger's normal playback period to 10 ms (480 frames), shrinking its
  pipe to FastMixer, and keeps the stock HAL at two periods. The capture ring
  holds eight periods (80 ms) for scheduling jitter; reads return as soon as
  their samples arrive.
- **Mixer names.** `StreamOut::updateMetadata` reports the highest-priority
  usage among active tracks (voice call, then game, media, assistant,
  navigation, notification or alarm, system sounds, unknown) with an empty
  label, so the bridge derives `media.name` and `media.role`. The PCM backend
  sends it only when the usage changes, and again after a reconnect. Capture
  stays unnamed: the bridge ignores `SET_METADATA` on record streams.
- **Capture.** Android's `RECORD_AUDIO` permission is the consent; the Flatpak's
  `--socket=pulseaudio` grants playback and capture. The bridge records the
  default source, or `INNKEEPER_AUDIO_SOURCE` (tests use a null-sink monitor).
  The input profile is 48 kHz s16, mono or stereo; AudioFlinger converts other
  formats. Module microphone mute zeroes captured samples. Standby closes the
  PCM and the host record stream; the next read opens a fresh ring.
- **No bridge.** Without a bridge, or after a stall or restart, output runs on a
  virtual clock and input returns paced silence, retrying once a second.
- **Stock HAL rc.** It preloads the session shim so priority changes are best
  effort in the rootless session; our service doesn't need that.

`mise run build:audio` builds and stages both services' artifacts. Host unit
tests cover output buffer sizes, position and latency arithmetic, usage
priority, FMQ wrapping and wakeups, and paced capture without a bridge.
`tests/abi.sh` checks the tinyalsa exports. The audio group of
`mise run test:e2e` plays a 440 Hz tone into a private null sink and checks its
frequency, then detects a known marker through AudioRecord from that sink's
monitor. `mise run bench -- audio` measures app-visible output latency and
capture round trip.
