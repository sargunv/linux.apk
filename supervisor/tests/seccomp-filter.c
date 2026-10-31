// Which syscalls does the session's seccomp filter (broker/src/filter.rs) turn
// into user notifications? Built and run by seccomp-filter.sh on the host.
// A child installs the filter and serves its listener; a grandchild makes each
// probe syscall, and the child records which ones arrived as notifications and
// lets them continue.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/seccomp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern int innkeeper_filter_install(void);

struct probe {
  const char* name;
  long nr;
  long a[6];
};

#define LSTAT_ARGS {AT_FDCWD, (long)"/", 0, AT_SYMLINK_NOFOLLOW}
static struct probe probes[] = {
  {"bpf", __NR_bpf, {-1}},
  {"openat", __NR_openat, {AT_FDCWD, (long)"/nonexistent", O_RDONLY}},
  {"ioctl binder", __NR_ioctl, {-1, 0xc0306201}},
  {"ioctl other", __NR_ioctl, {-1, 0x5401}},
  {"open", __NR_open, {(long)"/nonexistent", O_RDONLY}},
  {"ioctl access token", __NR_ioctl, {-1, 0x80084101}},
  {"ioctl ashmem", __NR_ioctl, {-1, 0x7704}},
  {"getuid", __NR_getuid},
  {"getgroups", __NR_getgroups, {0, 0}},
  {"setresuid", __NR_setresuid, {-1, -1, -1}},
  {"setgroups", __NR_setgroups, {0, 0}},
  {"fchownat", __NR_fchownat, {AT_FDCWD, (long)"/nonexistent", -1, -1, 0}},
  {"getsockopt SO_PEERCRED", __NR_getsockopt, {-1, SOL_SOCKET, SO_PEERCRED}},
  {"getsockopt SO_TYPE", __NR_getsockopt, {-1, SOL_SOCKET, SO_TYPE}},
  {"lstat", __NR_newfstatat, LSTAT_ARGS},
  {"access W_OK", __NR_faccessat, {AT_FDCWD, (long)"/", W_OK}},
  {"access R_OK", __NR_faccessat, {AT_FDCWD, (long)"/", R_OK}},
  {"stat", __NR_newfstatat, {AT_FDCWD, (long)"/", 0, 0}},
  {"fstat", __NR_fstat, {0, 0}},
  {"getpid", __NR_getpid},
  {"readlinkat", __NR_readlinkat, {AT_FDCWD, (long)"/nonexistent", 0, 0}},
  {"getcwd", __NR_getcwd, {0, 0}},
  {"setsockopt SO_MARK", __NR_setsockopt, {-1, SOL_SOCKET, SO_MARK}},
  {"setsockopt SO_RCVBUF", __NR_setsockopt, {-1, SOL_SOCKET, SO_RCVBUF}},
  {"capget", __NR_capget, {0, 0}},
  {"capset", __NR_capset, {0, 0}},
  {"mkdirat", __NR_mkdirat, {AT_FDCWD, (long)"/nonexistent/x", 0}},
  {"fchmodat", __NR_fchmodat, {AT_FDCWD, (long)"/nonexistent", 0}},
  {"prctl PR_CAPBSET_DROP", __NR_prctl, {PR_CAPBSET_DROP, 63}},
  {"prctl PR_SET_NAME", __NR_prctl, {PR_SET_NAME, 0}},
};
#define NPROBES (int)(sizeof(probes) / sizeof(probes[0]))

// Prints one line per probe: "<name> <0|1>" (1 = notified).
static int run(void) {
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 1;
  int lfd = innkeeper_filter_install();
  if (lfd < 0) return perror("filter"), 1;
  static struct stat st;
  for (int i = 0; i < NPROBES; i++)
    if (probes[i].nr == __NR_newfstatat || probes[i].nr == __NR_fstat)
      probes[i].a[probes[i].nr == __NR_fstat ? 1 : 2] = (long)&st;
  pid_t g = fork();
  if (g == 0) {
    for (int i = 0; i < NPROBES; i++) {
      // The parent matches notifications to probes by nr and arguments.
      long* a = probes[i].a;
      syscall(probes[i].nr, a[0], a[1], a[2], a[3], a[4], a[5]);
    }
    _exit(0);
  }
  int seen[NPROBES] = {0};
  struct seccomp_notif_sizes sz;
  syscall(__NR_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &sz);
  struct seccomp_notif* req = calloc(1, sz.seccomp_notif);
  struct seccomp_notif_resp* resp = calloc(1, sz.seccomp_notif_resp);
  for (;;) {
    int status;
    if (waitpid(g, &status, WNOHANG) == g) break;
    struct timeval tv = {0, 100000};
    fd_set fds;
    FD_ZERO(&fds);
    FD_SET(lfd, &fds);
    if (select(lfd + 1, &fds, NULL, NULL, &tv) <= 0) continue;
    memset(req, 0, sz.seccomp_notif);
    if (ioctl(lfd, SECCOMP_IOCTL_NOTIF_RECV, req)) continue;
    for (int i = 0; i < NPROBES; i++) {
      long* a = probes[i].a;
      if (req->data.nr != probes[i].nr) continue;
      if (
        probes[i].nr == __NR_ioctl && (long)(unsigned)req->data.args[1] != a[1]
      )
        continue;
      if (
        (probes[i].nr == __NR_getsockopt || probes[i].nr == __NR_setsockopt) &&
        (long)req->data.args[2] != a[2]
      )
        continue;
      if (probes[i].nr == __NR_prctl && (long)req->data.args[0] != a[0])
        continue;
      if (probes[i].nr == __NR_newfstatat && (long)req->data.args[3] != a[3])
        continue;
      if (probes[i].nr == __NR_faccessat && (long)req->data.args[2] != a[2])
        continue;
      seen[i] = 1;
    }
    memset(resp, 0, sz.seccomp_notif_resp);
    resp->id = req->id;
    resp->flags = SECCOMP_USER_NOTIF_FLAG_CONTINUE;
    ioctl(lfd, SECCOMP_IOCTL_NOTIF_SEND, resp);
  }
  // dprintf: stdio would fstat stdout, which the filter traps, under this
  // process's own filter.
  for (int i = 0; i < NPROBES; i++)
    dprintf(1, "%s %d\n", probes[i].name, seen[i]);
  return 0;
}

int main(void) { return run(); }
