#!/usr/bin/env bash
# Run the Vulkan/GLES test programs (built by build:vulkan into
# out/build/vulkan/test) through tests/bionic/run.sh, on lavapipe and, when
# selected by the launcher, on RADV or ANV.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
build=$repo/out/build/vulkan
[[ -x $build/test/vkdriver_test ]] || {
  echo "no Vulkan test programs in $build/test (run \`mise run build:vulkan\`)" >&2
  exit 1
}

driver_tests() {
  local d=$1 t=/data/nativetest64
  bionic() {
    "$repo/tests/bionic/run.sh" -t "$build/test" -v "$build/test-vendor" \
      -e "INNKEEPER_PROPS=ro.hardware.vulkan=$d;ro.hardware.egl=angle" "$@"
  }
  local vk=("$t/vkdriver_test" --hal "vulkan.$d.so")
  bionic -- "${vk[@]}" --udmabuf
  # --anb hands the driver a deliberately wrong stride: it passes only if the
  # layout came from the mapper (mock_mapper.c here, ours with --anb-hal).
  bionic -e MESA_GRALLOC_MAPPER_SUFFIX=inkptest -- "${vk[@]}" --anb
  bionic -e MESA_GRALLOC_MAPPER_SUFFIX=inkptest -- "${vk[@]}" --anb --deferred
  bionic -- "${vk[@]}" --anb-hal --deferred
  bionic -- "$t/vkdriver_test" --loader
  # fake_sf stands in for SurfaceFlinger's getDisplayStats.
  local sf="$t/fake_sf & until service check SurfaceFlingerAIDL | grep -q ': found'; do sleep 0.05; done"
  bionic -- /system/bin/sh -c "$sf; $t/swapchain_test"
  bionic -- /system/bin/sh -c "$sf; $t/swapchain_test --deferred"
  bionic -- "$t/ahb_vk_test"
  bionic -- "$t/gles_test"
  bionic -- "$t/gles_yuv_test"
}

# Ask a normal session, so the hardware test uses exactly the node and HAL
# selected by the launcher and permitted by its broker (also on hybrid hosts).
# shellcheck disable=SC2016 # getprop runs inside Android
selected=$("$repo/tests/bionic/run.sh" -- /system/bin/sh -c \
  'echo "innkeeper-selected-driver=$(getprop ro.hardware.vulkan)"' |
  sed -n 's/^innkeeper-selected-driver=//p')
drivers=(lvp)
case $selected in
  radv | anv) drivers+=("$selected") ;;
  lvp) ;;
  *)
    echo "unexpected selected Vulkan driver: $selected" >&2
    exit 1
    ;;
esac
for d in "${drivers[@]}"; do
  echo "== vulkan tests: $d"
  driver_tests "$d"
done
