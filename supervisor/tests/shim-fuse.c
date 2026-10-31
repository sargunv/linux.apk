// SPDX-License-Identifier: AGPL-3.0-or-later
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>

#include "../shim/fuse.h"

static int mount_volume(void) {
  int fd = fuse_open();
  assert(fd >= 0);
  struct {
    struct fuse_in_header header;
    struct fuse_init_in init;
  } request;
  struct pollfd ready = {.fd = fd, .events = POLLIN};
  assert(poll(&ready, 1, 100) == 1);
  assert(recv(fd, &request, sizeof(request), 0) == sizeof(request));
  assert(request.header.opcode == FUSE_INIT);
  assert(request.init.major == 7 && request.init.minor == 31);
  struct fuse_out_header reply = {.len = sizeof(reply), .unique = 1};
  assert(send(fd, &reply, sizeof(reply), MSG_NOSIGNAL) == sizeof(reply));
  return fd;
}

int main(void) {
  unsetenv("INNKEEPER_EMULATED_STORAGE");
  assert(fuse_open() == -1);
  setenv("INNKEEPER_EMULATED_STORAGE", "1", 1);
  for (int round = 0; round < 2; round++) {
    int fd = mount_volume();
    assert(fuse_open() == -1 && errno == ENODEV);
    // INIT must not close the peer: MediaProvider treats EOF as volume death.
    struct pollfd p = {.fd = fd, .events = POLLIN};
    assert(poll(&p, 1, 0) == 0);
    fuse_unmount("/mnt/other");
    assert(poll(&p, 1, 0) == 0);
    fuse_unmount(kEmulatedMount);
    assert(poll(&p, 1, 100) == 1);
    char byte;
    int result = recv(fd, &byte, 1, 0);
    assert(result == 0 || (result == -1 && errno == ECONNRESET));
    close(fd);
  }
  return 0;
}
