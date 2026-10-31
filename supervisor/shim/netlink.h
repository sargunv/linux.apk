// SPDX-License-Identifier: AGPL-3.0-or-later
#pragma once
#include <linux/netlink.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

// Concurrent sessions are separate pid namespaces on the host's network, so a
// uevent socket bound to its pid (vold's) collides with another session's. Let
// the kernel choose the port; other netlink protocols keep theirs. A header of
// its own for supervisor/tests/shim-netlink.c.
static int innkeeper_netlink_bind(
  int fd, const struct sockaddr* addr, socklen_t len
) {
  if (
    addr && len >= sizeof(struct sockaddr_nl) && addr->sa_family == AF_NETLINK
  ) {
    int protocol;
    socklen_t size = sizeof(protocol);
    if (
      getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &protocol, &size) == 0 &&
      protocol == NETLINK_KOBJECT_UEVENT
    ) {
      struct sockaddr_nl local = *(const struct sockaddr_nl*)addr;
      local.nl_pid = 0;
      return syscall(SYS_bind, fd, &local, sizeof(local));
    }
  }
  return syscall(SYS_bind, fd, addr, len);
}
