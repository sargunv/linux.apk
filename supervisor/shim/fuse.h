// SPDX-License-Identifier: AGPL-3.0-or-later
// vold only: complete MediaProvider's session handshake over the stable FUSE
// wire ABI. File I/O goes directly to /data/media through the launcher's links;
// there is no kernel mount, cache, or stream of filesystem requests to serve.
#include <linux/fuse.h>
#include <sys/socket.h>
#include <unistd.h>

// VolumeManager serializes mount/unmount. Only the primary emulated volume is
// supported; other FUSE users (AppFuse) must still fail rather than claim a
// mount.
static int fuse_peer = -1;
static const char kEmulatedMount[] = "/mnt/user/0/emulated";

static int fuse_enabled(void) {
  const char* e = getenv("INNKEEPER_EMULATED_STORAGE");
  return e && !strcmp(e, "1");
}

static int fuse_open(void) {
  if (!fuse_enabled()) return -1;
  if (fuse_peer != -1) {
    errno = ENODEV;
    return -1;
  }
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, pair)) return -1;
  struct {
    struct fuse_in_header header;
    struct fuse_init_in init;
  } request = {
    .header = {.len = sizeof(request), .opcode = FUSE_INIT, .unique = 1},
    .init = {.major = 7, .minor = 31, .max_readahead = 128 * 1024},
  };
  if (
    send(pair[1], &request, sizeof(request), MSG_NOSIGNAL) != sizeof(request)
  ) {
    int saved = errno;
    close(pair[0]);
    close(pair[1]);
    errno = saved;
    return -1;
  }
  // Keep the peer open until unmount so MediaProvider remains available for
  // volume scans and MediaStore operations. Its INIT reply fits in the socket;
  // no filesystem requests are sent because the storage paths are direct.
  fuse_peer = pair[1];
  return pair[0];
}

static void fuse_unmount(const char* target) {
  if (fuse_peer != -1 && !strcmp(target, kEmulatedMount)) {
    shutdown(fuse_peer, SHUT_RDWR);
    close(fuse_peer);
    fuse_peer = -1;
  }
}
