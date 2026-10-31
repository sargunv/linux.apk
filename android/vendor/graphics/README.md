# graphics

The display HALs, built out of tree with the NDK by `mise run build:graphics`
(the NDK superbuild) and staged under `share/innkeeper/android/vendor/`. init
starts the two services from `etc/init`; the mapper is loaded into every process
that touches gralloc buffers.

| Piece     | Interface                                                           | Installed as                                                   |
| --------- | ------------------------------------------------------------------- | -------------------------------------------------------------- |
| allocator | `android.hardware.graphics.allocator` AIDL V3, `IAllocator/default` | `bin/hw/android.hardware.graphics.allocator-service.innkeeper` |
| mapper    | stable-C IMapper v5, instance `innkeeper`                           | `lib64/hw/mapper.innkeeper.so`                                 |
| composer  | `android.hardware.graphics.composer3` AIDL V5, `IComposer/default`  | `bin/hw/android.hardware.graphics.composer3-service.innkeeper` |

- **Buffers** (`common/`, `allocator/gpu_buffer.cpp`): GPU-only RGB render and
  composer targets use the session's selected Vulkan driver to choose a
  single-plane tiled layout and export its allocation as a dma-buf. CPU access,
  cursor buffers, YUV and unsupported allocations keep the sealed memfd and
  `/dev/udmabuf` path. The native handle has two fds: the pixels, then a sealed
  memfd whose 4 KiB shared metadata page carries the DRM modifier alongside
  dataspace and HDR metadata, followed by any client reserved region. Keeping
  metadata separate lets a driver's dedicated allocation hold just the pixels.
  Compression with auxiliary planes is excluded. Pitch and YUV rules are in
  [docs/display.md](../../../docs/display.md#buffers). Only the session's root
  may open `/dev/udmabuf`, so the allocator opens it before dropping to
  `system:graphics`.
- **Composer:** one headless display 0 (1280x800) plus up to 32 HWC-backed
  virtual displays. Content layers are CLIENT; CURSOR layers go to the Wayland
  cursor. `vendor.innkeeper.hwc.backend` picks the presenter (`Presenter.cpp`):
  `bridge` (the session default) sends virtual displays to the bridge's display
  service, `headless` shows nothing. `vendor.innkeeper.hwc.dump.dir` writes PNG
  frame dumps.
- **Vsync:** display 0 supplies Android's scheduling clock. It follows the
  refresh and `wp_presentation` phase of a visible mapped window's output,
  keeping that phase while the window is static, and falls back to a 60 Hz timer
  without one; `vendor.innkeeper.hwc.host_clock=0` forces the timer. The rc sets
  app and SF work durations to 4 ms each, to fit a 120 Hz frame.
- **LL-NDK only.** `third_party/aosp` holds the frozen AIDL snapshots and binder
  NDK and IMapper headers (`tools/sync-aosp.sh`); `tools/gen-aidl.sh` generates
  the NDK backend with the build-tools' `aidl`, and `tools/gen-stub.py` turns
  AOSP's `libbinder_ndk.map.txt` into a link-only stub for the platform symbols.
  GPU allocation uses LL-NDK libvulkan; its native handle and usage policy stay
  in our allocator, without a GBM/minigbm allocator or another gralloc HAL.

Tests: `tests/host` runs in `mise run test` (layouts, every format, the mapper
through `dlopen`, metadata across imports and `fork`, the frame dumper);
`tests/android` (`gralloc_test`, `hwc_test`) runs in `mise run test:bionic` in a
session with the framework stopped. Start reading at `common/src/layout.cpp`,
`composer/Display.cpp` and `composer/BridgePresenter.cpp`.
