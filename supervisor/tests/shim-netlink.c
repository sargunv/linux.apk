// SPDX-License-Identifier: AGPL-3.0-or-later
#include <assert.h>
#include <errno.h>
#include <stdio.h>

#include "../shim/netlink.h"

static void check(int protocol, int allocated) {
  int a = socket(AF_NETLINK, SOCK_DGRAM, protocol);
  int b = socket(AF_NETLINK, SOCK_DGRAM, protocol);
  assert(a >= 0 && b >= 0);
  struct sockaddr_nl address = {.nl_family = AF_NETLINK, .nl_pid = getpid()};
  assert(innkeeper_netlink_bind(a, (void*)&address, sizeof(address)) == 0);
  int rc = innkeeper_netlink_bind(b, (void*)&address, sizeof(address));
  if (allocated) {
    assert(rc == 0);
    struct sockaddr_nl first, second;
    socklen_t size = sizeof(first);
    assert(getsockname(a, (void*)&first, &size) == 0);
    size = sizeof(second);
    assert(getsockname(b, (void*)&second, &size) == 0);
    assert(first.nl_pid != second.nl_pid);
    assert(address.nl_pid == (unsigned)getpid());
  } else {
    assert(rc == -1 && errno == EADDRINUSE);
  }
  close(a);
  close(b);
}

int main(void) {
  check(NETLINK_KOBJECT_UEVENT, 1);
  check(NETLINK_USERSOCK, 0);
  puts("ok   uevent receivers allocate ports; request/reply binds keep theirs");
}
