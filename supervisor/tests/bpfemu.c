// Existing bpf syscall probes against the Rust broker (broker/src/bpf.rs).
// The child inherits the real filter; the parent runs the real broker.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

extern int innkeeper_filter_install(void);
extern void innkeeper_broker_start(int listener, int pins);

static int fails;
#define CHECK(cond, ...)           \
  do {                             \
    if (cond) {                    \
      printf("ok   " __VA_ARGS__); \
    } else {                       \
      printf("FAIL " __VA_ARGS__); \
      fails++;                     \
    }                              \
    printf("\n");                  \
    fflush(stdout);                \
  } while (0)

// bpf(): the result, or -errno.
static long bpf(int cmd, union bpf_attr* a) {
  long r = syscall(__NR_bpf, cmd, a, sizeof(*a));
  return r < 0 ? -errno : r;
}

static long map_create(
  uint32_t type, uint32_t key, uint32_t val, uint32_t max
) {
  union bpf_attr a = {0};
  a.map_type = type;
  a.key_size = key;
  a.value_size = val;
  a.max_entries = max;
  return bpf(BPF_MAP_CREATE, &a);
}

static long update(int fd, const void* key, const void* val) {
  union bpf_attr a = {0};
  a.map_fd = fd;
  a.key = (uintptr_t)key;
  a.value = (uintptr_t)val;
  return bpf(BPF_MAP_UPDATE_ELEM, &a);
}

static long lookup(int fd, const void* key, void* val) {
  union bpf_attr a = {0};
  a.map_fd = fd;
  a.key = (uintptr_t)key;
  a.value = (uintptr_t)val;
  return bpf(BPF_MAP_LOOKUP_ELEM, &a);
}

static long pin(int fd, const char* path) {
  union bpf_attr a = {0};
  a.bpf_fd = fd;
  a.pathname = (uintptr_t)path;
  return bpf(BPF_OBJ_PIN, &a);
}

static long get(const char* path) {
  union bpf_attr a = {0};
  a.pathname = (uintptr_t)path;
  return bpf(BPF_OBJ_GET, &a);
}

static uint32_t map_id(int fd) {
  struct bpf_map_info info = {0};
  union bpf_attr a = {0};
  a.info.bpf_fd = fd;
  a.info.info_len = sizeof(info);
  a.info.info = (uintptr_t)&info;
  return bpf(BPF_OBJ_GET_INFO_BY_FD, &a) == 0 ? info.id : 0;
}

static void tests(const char* outside_dir) {
  // A working map, pinned and fetched back.
  long m = map_create(BPF_MAP_TYPE_HASH, 4, 8, 16);
  uint32_t k = 7;
  uint64_t v = 42, got = 0;
  CHECK(m >= 0 && update(m, &k, &v) == 0, "hash map create and update");
  CHECK(pin(m, "/sys/fs/bpf/m") == 0, "pin under /sys/fs/bpf");
  long g = get("/sys/fs/bpf/m");
  CHECK(g >= 0 && lookup(g, &k, &got) == 0 && got == 42, "get pin and lookup");

  // netd's mapLockTest needs one inode but independent open descriptions.
  long g2 = get("/sys/fs/bpf/m");
  struct stat ms, gs;
  CHECK(
    g2 >= 0 && fstat(m, &ms) == 0 && fstat(g2, &gs) == 0 &&
      ms.st_dev == gs.st_dev && ms.st_ino == gs.st_ino,
    "map opens share one inode"
  );
  struct flock lock = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
  CHECK(fcntl(g, F_OFD_SETLK, &lock) == 0, "map read OFD lock");
  lock.l_type = F_WRLCK;
  CHECK(
    fcntl(g2, F_OFD_SETLK, &lock) == -1 && errno == EAGAIN,
    "independent map open conflicts with OFD lock"
  );
  close(g);
  CHECK(
    fcntl(g2, F_OFD_SETLK, &lock) == 0, "closing map open releases OFD lock"
  );
  close(g2);

  // S3: element operations on ring buffers (no tables).
  long ur = map_create(BPF_MAP_TYPE_USER_RINGBUF, 0, 0, 4096);
  long rb = map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, 4096);
  CHECK(ur >= 0 && rb >= 0, "ring buffer create");
  CHECK(update(ur, &k, &v) == -EINVAL, "user ring buffer update: EINVAL");
  CHECK(lookup(rb, &k, &got) == -EINVAL, "ring buffer lookup: EINVAL");

  // S3: LPM prefixes must fit the key.
  CHECK(
    map_create(BPF_MAP_TYPE_LPM_TRIE, 4, 8, 16) == -EINVAL,
    "LPM key without data: EINVAL"
  );
  long lpm = map_create(BPF_MAP_TYPE_LPM_TRIE, 8, 8, 16);
  struct {
    uint32_t plen;
    uint8_t data[4];
  } key = {33, {10, 0, 0, 1}};
  CHECK(
    lpm >= 0 && update(lpm, &key, &v) == -EINVAL,
    "LPM prefix 33 in 4 bytes: EINVAL"
  );
  key.plen = 32;
  CHECK(
    update(lpm, &key, &v) == 0 && lookup(lpm, &key, &got) == 0, "LPM prefix 32"
  );

  // S6: pins are regular files under /sys/fs/bpf, opened without blocking or
  // following links out of it.
  mkfifo("/sys/fs/bpf/fifo", 0600);
  CHECK(get("/sys/fs/bpf/fifo") == -EINVAL, "get of a FIFO: EINVAL, no hang");
  char esc[512];
  snprintf(esc, sizeof(esc), "%s/escaped", outside_dir);
  CHECK(
    symlink(outside_dir, "/sys/fs/bpf/link") == 0, "symlink into the pin dir"
  );
  CHECK(
    pin(m, "/sys/fs/bpf/link/escaped") == -ELOOP && access(esc, F_OK) != 0,
    "pin through a symlink: ELOOP, nothing created outside"
  );
  CHECK(symlink(esc, "/sys/fs/bpf/flink") == 0, "symlink to a file outside");
  CHECK(get("/sys/fs/bpf/flink") == -ELOOP, "get of a symlink: ELOOP");
  CHECK(get("/sys/fs/bpf/../../../etc/passwd") == -EXDEV, "get with ..: EXDEV");
  char outside[512];
  snprintf(outside, sizeof(outside), "%s/m", outside_dir);
  CHECK(pin(m, outside) == -EINVAL, "pin outside /sys/fs/bpf: EINVAL");
  struct statfs sfs;
  CHECK(
    statfs("/sys/fs/bpf/m", &sfs) == 0 && sfs.f_type == 0xcafe4a11,
    "statfs of a pin: bpffs magic"
  );
  // The supervisor's pin directory was fixed at setup: a mount over
  // /sys/fs/bpf (FUSE, in a session) changes nothing it opens.
  if (mount("over", "/sys/fs/bpf", "tmpfs", 0, NULL) == 0) {
    CHECK(
      pin(m, "/sys/fs/bpf/over") == 0 && access("/sys/fs/bpf/over", F_OK) != 0,
      "pin after a mount over /sys/fs/bpf: in the session's directory"
    );
    long o = get("/sys/fs/bpf/over");
    CHECK(o >= 0 && lookup(o, &k, &got) == 0, "get after the over-mount");
    umount2("/sys/fs/bpf", MNT_DETACH);
  } else {
    CHECK(0, "mount over /sys/fs/bpf: %s", strerror(errno));
  }

  // S7: impossible sizes fail up front, and failures leave nothing behind.
  uint32_t before = map_id(lpm);  // the newest map
  CHECK(
    map_create(BPF_MAP_TYPE_HASH, 4, 4096, 1u << 24) == -ENOMEM,
    "64 GiB map: ENOMEM"
  );
  CHECK(
    map_create(BPF_MAP_TYPE_PERCPU_HASH, 4, 1u << 30, 1) == -E2BIG,
    "huge per-CPU value: E2BIG"
  );
  long next = map_create(BPF_MAP_TYPE_ARRAY, 4, 4, 1);
  uint32_t last = map_id(next);
  CHECK(
    before && last == before + 1,
    "failed creations unregistered (id %u after %u)", last, before
  );

  // S7: aggregate map memory is capped (64 MiB each).
  int i;
  for (i = 0; i < 64; i++) {
    long f = map_create(BPF_MAP_TYPE_ARRAY, 4, 16384, 4096);
    if (f < 0) break;
    close(f);
  }
  CHECK(i < 64, "map memory capped after %d x 64 MiB", i);

  // S7: the object count is capped.
  for (i = 0; i < 5000; i++) {
    union bpf_attr a = {0};
    long f = bpf(BPF_PROG_LOAD, &a);
    if (f < 0) break;
    close(f);
  }
  CHECK(i < 5000, "objects capped after %d programs", i);
}

int main(int argc, char** argv) {
  if (argc < 2) return fprintf(stderr, "usage: %s OUTSIDE_DIR\n", argv[0]), 2;
  // Room for more objects than the cap (each holds a memfd).
  struct rlimit rl;
  getrlimit(RLIMIT_NOFILE, &rl);
  rl.rlim_cur = rl.rlim_max;
  setrlimit(RLIMIT_NOFILE, &rl);
  // As pivot-exec does before Android runs.
  int pins = open("/sys/fs/bpf", O_PATH | O_DIRECTORY | O_CLOEXEC);
  int sp[2];
  if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sp))
    return perror("socketpair"), 1;
  pid_t c = fork();
  if (c == 0) {
    alarm(20);
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 1;
    int lfd = innkeeper_filter_install();
    if (lfd < 0) return perror("filter"), 1;
    char ctl[CMSG_SPACE(sizeof(int))] = {0};
    char b = 0;
    struct iovec io = {&b, 1};
    struct msghdr mh = {
      .msg_iov = &io,
      .msg_iovlen = 1,
      .msg_control = ctl,
      .msg_controllen = sizeof(ctl)
    };
    struct cmsghdr* cm = CMSG_FIRSTHDR(&mh);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &lfd, sizeof(int));
    if (sendmsg(sp[1], &mh, 0) != 1) return 1;
    close(lfd);
    tests(argv[1]);
    return fails ? 1 : 0;
  }
  char ctl[CMSG_SPACE(sizeof(int))];
  char b;
  struct iovec io = {&b, 1};
  struct msghdr mh = {
    .msg_iov = &io,
    .msg_iovlen = 1,
    .msg_control = ctl,
    .msg_controllen = sizeof(ctl)
  };
  if (recvmsg(sp[0], &mh, 0) != 1 || !CMSG_FIRSTHDR(&mh))
    return fprintf(stderr, "no listener\n"), 1;
  int listener;
  memcpy(&listener, CMSG_DATA(CMSG_FIRSTHDR(&mh)), sizeof(int));
  innkeeper_broker_start(listener, pins);
  // A hung or crashed supervisor fails the test (SIGALRM, SIGSEGV).
  alarm(30);
  int st;
  waitpid(c, &st, 0);
  if (!WIFEXITED(st))
    return fprintf(
             stderr, "FAIL child killed by signal %d (hang?)\n", WTERMSIG(st)
           ),
           1;
  return WEXITSTATUS(st);
}
