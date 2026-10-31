# vulkan

The vendor Vulkan drivers: Mesa (26.2.5) RADV for AMD GPUs, ANV for supported
Gen9+ i915 Intel GPUs and lavapipe for everything else, cross-built with the NDK
by `mise run build:vulkan` (`build.sh`, in the build container; skipped while
its inputs hash to `out/build/vulkan/.stamp`; the first LLVM build takes about
15 minutes). They install as `/vendor/lib64/hw/vulkan.{radv,anv,lvp}.so`, and
libvulkan loads one into every process that uses Vulkan. GLES is the image's
ANGLE on top (`ro.hardware.egl=angle`, in `etc/vulkan.prop`).

- **One self-contained `.so` per driver.** libdrm, zlib, LLVM (lavapipe only;
  RADV uses ACO and ANV its own compiler) and libc++ are linked statically;
  `DT_NEEDED` is LL-NDK only and the only export is `HMI`. `build.sh` checks
  both.
- **Native shader tools:** ANV's build needs `mesa_clc` and `vtn_bindgen2`,
  built from the same Mesa source against the container's LLVM/Clang and never
  shipped. Intel ray tracing is disabled.
- **Driver choice:** the launcher scans `/sys/class/drm/renderD*` and picks the
  first amdgpu node (RADV), then an i915 node ANV supports, else lavapipe.
  `share/innkeeper/anv-pci-ids` is generated from the pinned Mesa's device info,
  PCI table and ANV generation limits, so older and unknown Intel IDs fall back
  to software. `INNKEEPER_VULKAN=lvp` forces software rendering. The choice is
  saved in session.conf, and the broker lets Android open only that DRM device,
  so Mesa and the composer's ANGLE use the same GPU.
- **Features:** the launcher writes Vulkan permission XMLs into the runtime
  layer alongside `ro.hardware.vulkan`: Vulkan 1.3, compute level 0, and
  hardware level 1 for RADV/ANV or 0 for lavapipe.
- **`-Dandroid-strict=false`** on purpose: strict mode caps the drivers at
  Vulkan 1.1 and the CTS extension allowlist. All three report Vulkan 1.4.
- **What the drivers need from the gralloc:** IMapper v5 metadata
  (`PIXEL_FORMAT_FOURCC`, `PLANE_LAYOUTS`, `DATASPACE`) and the 256-byte pitch
  ([docs/display.md](../../../docs/display.md#buffers)). GPU render targets are
  driver-allocated and tiled (dedicated allocations where the driver requires
  them, as RADV does); CPU-accessed buffers stay on udmabuf. The mapper supplies
  the modifier and explicit layout for LINEAR and tiled imports, so Mesa needs
  no Intel tiling or CCS override. Release fences are real sync_files from DRM
  syncobjs, so no sw_sync is needed.
- **Patches** (`patches/mesa/`, each with its rationale in the header): a
  stable-C IMapper v5 gralloc backend so the drivers link no libui or
  libhardware (0001); lavapipe's ICD version script (0002); deferred-allocation
  swapchain images that ANGLE uses, which crashed bootanimation on RADV (0003,
  0004); YUV hardware-buffer formats through the gralloc, without which video
  aborted SurfaceFlinger (0005); ANV's ANB imports use the mapper's explicit row
  pitches and offsets without requiring the optional AHardwareBuffer pointer
  (0006).

Tests: `tests/run.sh` (in `mise run test:bionic`) runs `vkdriver_test` (loading,
compute, udmabuf import, ANB images, deferred binds), `swapchain_test`,
`ahb_vk_test`, `gles_test` and `gles_yuv_test` (YV12, NV12, NV21 through
EGLImage as Skia imports them) on lavapipe plus the hardware driver a normal
launcher session selects. On hybrids that is only the selected GPU, since the
broker permits only its node. Each invocation uses a fresh named Android
session.
