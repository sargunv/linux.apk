#!/usr/bin/env bash
# Hosted runners only: never run privileged setup on a contributor's machine.
set -euo pipefail
[[ ${GITHUB_ACTIONS:-} == true ]] || {
  echo 'GitHub runner required' >&2
  exit 1
}
sudo apt-get update -qq
mode=${INNKEEPER_CI_MODE:?}
if [[ $mode != e2e ]]; then
  sudo apt-get install -y --no-install-recommends \
    clang ccache pkg-config libgtk-4-dev libadwaita-1-dev libpulse-dev libxkbcommon-dev
fi
if [[ $mode == build ]]; then
  sudo apt-get install -y --no-install-recommends podman uidmap passt
fi
if [[ $mode == build || $mode == unit || $mode == e2e || $mode == package ]]; then
  # The build container, supervisor unit tests and Flatpak need unprivileged mount namespaces.
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi
if [[ $mode == unit || $mode == e2e || $mode == package ]]; then
  # Gralloc's host unit tests also exercise real dma-buf allocations.
  if [[ ! -e /dev/udmabuf ]]; then
    sudo modprobe udmabuf
  fi
  sudo chmod 666 /dev/udmabuf
  test -r /dev/udmabuf && test -w /dev/udmabuf
fi
[[ $mode == e2e || $mode == package ]] || exit 0
sudo apt-get install -y --no-install-recommends \
  podman uidmap passt flatpak fuse3 cpio dbus-user-session gnome-shell mutter \
  xdg-desktop-portal xdg-desktop-portal-gtk \
  at-spi2-core python3-pyatspi python3-dbus python3-gi wl-clipboard \
  libgl1-mesa-dri mesa-vulkan-drivers pipewire pipewire-pulse wireplumber pulseaudio-utils
XDG_RUNTIME_DIR=/run/user/$(id -u)
export XDG_RUNTIME_DIR
sudo loginctl enable-linger "$USER"
sudo systemctl start "user@$(id -u).service"
systemctl --user start pipewire pipewire-pulse wireplumber
# Mutter uses surfaceless llvmpipe and advertises linux-dmabuf without a DRM node.
systemctl --user set-environment LIBGL_ALWAYS_SOFTWARE=1
{
  echo "INNKEEPER_TEST_COMPOSITOR=mutter"
  echo "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
  echo "DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus"
} >>"$GITHUB_ENV"
