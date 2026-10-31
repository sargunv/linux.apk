// LD_PRELOAD shim for calls an unmodified Android userspace makes that the
// Flatpak sandbox refuses or a host kernel answers differently
// (supervisor/README.md, "The shim").
//
// SELinux: the session hides /sys/fs/selinux, so libselinux reports SELinux as
// disabled and skips access checks, restorecon and app domain transitions. A
// few call sites in init still assume an Android policy is loaded:
//
//   * Service::Start computes a domain with security_compute_create(), which
//     needs selinuxfs and fails, so no service without a seclabel can start.
//   * The forked child calls setexeccon(seclabel); the host kernel's SELinux
//     rejects Android contexts with EINVAL, and init treats that as fatal.
//   * Sockets and files get setsockcreatecon()/setfscreatecon() contexts.
//
// Those calls succeed as no-ops. File contexts fall back to a synthetic label
// on hosts without SELinux. Process and peer contexts keep their host labels,
// used only in logs and checks libselinux already skips.
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/netlink.h>
#include <sched.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#define EXPORT __attribute__((visibility("default")))

typedef unsigned short security_class_t;

static const char kServiceContext[] = "u:r:innkeeper:s0";

#include "selinux-file.h"

EXPORT int security_compute_create(
  const char* scon, const char* tcon, security_class_t tclass, char** newcon
) {
  (void)scon;
  (void)tcon;
  (void)tclass;
  *newcon = strdup(kServiceContext);
  return *newcon ? 0 : -1;
}
EXPORT int security_compute_create_raw(
  const char* scon, const char* tcon, security_class_t tclass, char** newcon
) {
  return security_compute_create(scon, tcon, tclass, newcon);
}

#define NOOP_SETTER(name)                  \
  EXPORT int name(const char* con) {       \
    (void)con;                             \
    return 0;                              \
  }                                        \
  EXPORT int name##_raw(const char* con) { \
    (void)con;                             \
    return 0;                              \
  }

NOOP_SETTER(setcon)
NOOP_SETTER(setexeccon)
NOOP_SETTER(setfscreatecon)
NOOP_SETTER(setsockcreatecon)
NOOP_SETTER(setkeycreatecon)

#define NOOP_FILE_SETTER(name, arg_t)               \
  EXPORT int name(arg_t f, const char* con) {       \
    (void)f;                                        \
    (void)con;                                      \
    return 0;                                       \
  }                                                 \
  EXPORT int name##_raw(arg_t f, const char* con) { \
    (void)f;                                        \
    (void)con;                                      \
    return 0;                                       \
  }

NOOP_FILE_SETTER(setfilecon, const char*)
NOOP_FILE_SETTER(lsetfilecon, const char*)
NOOP_FILE_SETTER(fsetfilecon, int)

// selinux_status_*: the status page lives in selinuxfs, which is hidden.
// servicemanager CHECKs that selinux_status_open() succeeds. Report a
// status that never changes (no policy reloads, permissive).
EXPORT int selinux_status_open(int fallback) {
  (void)fallback;
  return 0;
}
EXPORT void selinux_status_close(void) {}
EXPORT int selinux_status_updated(void) { return 0; }
EXPORT int selinux_status_getenforce(void) { return 0; }
EXPORT int selinux_status_policyload(void) { return 0; }
EXPORT int selinux_status_deny_unknown(void) { return 0; }

// Labelling files by writing the security.selinux xattr directly (bionic's
// property area setup, restorecon fallbacks): the host kernel rejects Android
// contexts, and bionic's __system_property_area_init() fails, which is fatal
// for init. Pretend success for that one xattr; pass everything else through.
static int is_selinux_xattr(const char* name) {
  return name && strcmp(name, "security.selinux") == 0;
}

// The kernel rejects POSIX ACL entries
// naming Android's uids and gids, which aren't mapped. vold gives
// /data/media/<user> a default ACL (group media_rw/everybody) and fails user
// storage preparation without it, and system_server then reboots to recovery
// to wipe /data. Every Android process is the same kernel uid there, so the
// ACL has nothing to grant: skip it.
static int is_skipped_xattr(const char* name) {
  if (is_selinux_xattr(name)) return 1;
  return name && !strncmp(name, "system.posix_acl_", 17);
}

EXPORT int fsetxattr(
  int fd, const char* name, const void* value, size_t size, int flags
) {
  if (is_skipped_xattr(name)) return 0;
  return syscall(SYS_fsetxattr, fd, name, value, size, flags);
}
EXPORT int setxattr(
  const char* path, const char* name, const void* value, size_t size, int flags
) {
  if (is_skipped_xattr(name)) return 0;
  return syscall(SYS_setxattr, path, name, value, size, flags);
}
EXPORT int lsetxattr(
  const char* path, const char* name, const void* value, size_t size, int flags
) {
  if (is_skipped_xattr(name)) return 0;
  return syscall(SYS_lsetxattr, path, name, value, size, flags);
}

// Resource limits and priorities. In a user namespace, raising a hard
// rlimit, lowering nice below the host's RLIMIT_NICE, or picking RT policies
// needs capabilities in the initial namespace. init treats a failed
// setrlimit/setpriority in a service's "rlimit"/"priority" option as fatal
// for that service (zygote has "priority -20"). Clamp limits to what we may
// set and treat priority changes we aren't allowed to make as best-effort.

EXPORT int setpriority(int which, id_t who, int prio) {
  long rc = syscall(SYS_setpriority, which, who, prio);
  if (rc != 0 && (errno == EACCES || errno == EPERM)) return 0;
  return rc;
}

EXPORT int prlimit64(
  pid_t pid, int resource, const struct rlimit64* new_limit,
  struct rlimit64* old_limit
) {
  long rc = syscall(SYS_prlimit64, pid, resource, new_limit, old_limit);
  if (rc != 0 && errno == EPERM && new_limit) {
    struct rlimit64 cur, clamped = *new_limit;
    if (syscall(SYS_prlimit64, pid, resource, NULL, &cur) != 0) return -1;
    if (clamped.rlim_max > cur.rlim_max) clamped.rlim_max = cur.rlim_max;
    if (clamped.rlim_cur > clamped.rlim_max)
      clamped.rlim_cur = clamped.rlim_max;
    rc = syscall(SYS_prlimit64, pid, resource, &clamped, old_limit);
  }
  return rc;
}
EXPORT int setrlimit64(int resource, const struct rlimit64* limit) {
  return prlimit64(0, resource, limit, NULL);
}
EXPORT int setrlimit(int resource, const struct rlimit* limit) {
  struct rlimit64 l = {limit->rlim_cur, limit->rlim_max};
  return prlimit64(0, resource, &l, NULL);
}
EXPORT int prlimit(
  pid_t pid, int resource, const struct rlimit* new_limit,
  struct rlimit* old_limit
) {
  struct rlimit64 n, o;
  if (new_limit) {
    n.rlim_cur = new_limit->rlim_cur;
    n.rlim_max = new_limit->rlim_max;
  }
  int rc =
    prlimit64(pid, resource, new_limit ? &n : NULL, old_limit ? &o : NULL);
  if (rc == 0 && old_limit) {
    old_limit->rlim_cur = o.rlim_cur;
    old_limit->rlim_max = o.rlim_max;
  }
  return rc;
}

// Scope: the shim is LD_PRELOADed into init only. It has to be loaded by
// path, and no single path is visible from every linker namespace (vendor
// and APEX namespaces reject /system/lib64, and a bare name is only found in
// some of them), so letting init's services inherit LD_PRELOAD makes some of
// them fail to link. init's forked children still run the shim between fork
// and exec, which is where init needs it. Services that need it themselves
// get "setenv LD_PRELOAD /system/lib64/libinnkeeper_shim.so" in their rc.
// In a Flatpak session init isn't pid 1; pivot-exec marks it with
// INNKEEPER_SHIM_INIT instead.
__attribute__((constructor)) static void innkeeper_shim_init(void) {
  if (syscall(SYS_getpid) == 1 || getenv("INNKEEPER_SHIM_INIT")) {
    unsetenv("LD_PRELOAD");
    unsetenv("INNKEEPER_SHIM_INIT");
  }
}

// The sandbox's seccomp filter fails mount, umount,
// unshare and setns with EPERM, ahead of any filter of ours. Android's root is
// already assembled (symlinks, see host/launcher/src/flatpak.rs) and has no
// namespaces to make, so a refused call is treated as done: init's mounts,
// zygote's per-app mount namespace and storage views, vold's binds.
#include "fuse.h"

static int do_open(const char* path, int flags, mode_t mode) {
  int fd = syscall(SYS_openat, AT_FDCWD, path, flags, mode);
  if (fd == -1 && errno == ENOENT && !strcmp(path, "/dev/fuse"))
    return fuse_open();
  return fd;
}
EXPORT int open(const char* path, int flags, ...) {
  mode_t mode = 0;
  if ((flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE) {
    va_list ap;
    va_start(ap, flags);
    mode = va_arg(ap, int);
    va_end(ap);
  }
  return do_open(path, flags, mode);
}
EXPORT int __open_2(const char* path, int flags) {
  if ((flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE) abort();
  return do_open(path, flags, 0);
}

static long refused_ok(long rc) { return rc != 0 && errno == EPERM ? 0 : rc; }
EXPORT int mount(
  const char* src, const char* dst, const char* type, unsigned long flags,
  const void* data
) {
  if (
    fuse_peer != -1 && type && !strcmp(type, "fuse") &&
    strcmp(dst, kEmulatedMount)
  ) {
    fuse_unmount(kEmulatedMount);
    errno = ENODEV;
    return -1;
  }
  return refused_ok(syscall(SYS_mount, src, dst, type, flags, data));
}
EXPORT int umount2(const char* target, int flags) {
  fuse_unmount(target);
  return refused_ok(syscall(SYS_umount2, target, flags));
}
EXPORT int umount(const char* target) { return umount2(target, 0); }
EXPORT int unshare(int flags) {
  return refused_ok(syscall(SYS_unshare, flags));
}
EXPORT int setns(int fd, int nstype) {
  return refused_ok(syscall(SYS_setns, fd, nstype));
}
// netd (its rc sets INNKEEPER_NETLINK_ACCEPT=1): the session
// shares the host's network namespace, where netd may change nothing. The
// kernel answers its rtnetlink requests (rules, routes, addresses) with
// EPERM, and netd won't start without its routing rules: report them done.
// Its routing then means nothing, as its marks (SO_MARK, bpfemu) and
// firewall rules don't; sockets use the host's routes,
// as every desktop app does.
static int netlink_accept_state = -1;
static void netlink_accept(int fd, void* buf, long n) {
  if (n <= 0) return;
  if (netlink_accept_state < 0) {
    const char* e = getenv("INNKEEPER_NETLINK_ACCEPT");
    netlink_accept_state = e && !strcmp(e, "1");
  }
  if (!netlink_accept_state) return;
  int domain;
  socklen_t l = sizeof(domain);
  if (
    getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &domain, &l) != 0 ||
    domain != AF_NETLINK
  )
    return;
  // An ack may be cut short: netd reads just the header and error code.
  char* p = buf;
  while (n >= (long)(NLMSG_HDRLEN + sizeof(int))) {
    struct nlmsghdr* h = (struct nlmsghdr*)p;
    if (h->nlmsg_type == NLMSG_ERROR) {
      struct nlmsgerr* e = NLMSG_DATA(h);
      if (e->error == -EPERM || e->error == -EACCES) e->error = 0;
    }
    long step = NLMSG_ALIGN(h->nlmsg_len);
    if (h->nlmsg_len < NLMSG_HDRLEN || step > n) break;
    p += step;
    n -= step;
  }
}
EXPORT ssize_t recvfrom(
  int fd, void* buf, size_t len, int flags, struct sockaddr* addr,
  socklen_t* alen
) {
  long n = syscall(SYS_recvfrom, fd, buf, len, flags, addr, alen);
  netlink_accept(fd, buf, n);
  return n;
}
EXPORT ssize_t recv(int fd, void* buf, size_t len, int flags) {
  return recvfrom(fd, buf, len, flags, NULL, NULL);
}
// vold's uevent socket (netlink.h).
#include "netlink.h"
EXPORT int bind(int fd, const struct sockaddr* addr, socklen_t len) {
  return innkeeper_netlink_bind(fd, addr, len);
}
// netdutils reads XFRM acks with read().
EXPORT ssize_t read(int fd, void* buf, size_t len) {
  long n = syscall(SYS_read, fd, buf, len);
  if (n > 0 && netlink_accept_state != 0) netlink_accept(fd, buf, n);
  return n;
}
// What FORTIFY makes of recv() with a known buffer size.
EXPORT ssize_t __recvfrom_chk(
  int fd, void* buf, size_t len, size_t buf_size, int flags,
  struct sockaddr* addr, socklen_t* alen
) {
  if (len > buf_size) abort();
  return recvfrom(fd, buf, len, flags, addr, alen);
}

// lmkd (its rc sets INNKEEPER_PSI_WINDOW=1): the kernel takes
// PSI triggers with a window under 2 s (lmkd asks for 1 s) only from a
// process with CAP_SYS_RESOURCE, which the sandbox never has. Widen the
// window to 2 s; lmkd still polls the same memory pressure.
static int psi_window_state = -1;
static long psi_write(int fd, const void* buf, size_t n) {
  char kind[8], link[64], path[32], out[64];
  unsigned stall, window;
  if (psi_window_state < 0) {
    const char* e = getenv("INNKEEPER_PSI_WINDOW");
    psi_window_state = e && !strcmp(e, "1");
  }
  if (
    !psi_window_state || n < 8 || n > 48 ||
    (memcmp(buf, "some ", 5) && memcmp(buf, "full ", 5))
  )
    return -2;
  memcpy(out, buf, n);
  out[n] = 0;
  if (sscanf(out, "%4s %u %u", kind, &stall, &window) != 3) return -2;
  snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
  long l = syscall(SYS_readlinkat, AT_FDCWD, link, path, sizeof(path) - 1);
  if (l <= 0) return -2;
  path[l] = 0;
  if (strncmp(path, "/proc/pressure/", 15) || window % 2000000 == 0) return -2;
  window = (window / 2000000 + 1) * 2000000;
  int m = snprintf(out, sizeof(out), "%s %u %u", kind, stall, window);
  return syscall(SYS_write, fd, out, m + 1) < 0 ? -1 : (long)n;
}

#include "oom.h"

// Only lmkd opts in. Keep its Android scores for its own kill policy, but
// write kernel scores that it can lower again when an app becomes foreground.
static long oom_write(int fd, const void* buf, size_t n) {
  static int floor = -2;
  if (floor == -2) {
    floor = -1;
    const char* e = getenv("INNKEEPER_OOM_ADJ");
    if (e && !strcmp(e, "1")) {
      int f =
        syscall(SYS_openat, AT_FDCWD, "/proc/self/oom_score_adj", O_RDONLY);
      if (f >= 0) {
        char value[16];
        long l = syscall(SYS_read, f, value, sizeof(value) - 1);
        syscall(SYS_close, f);
        if (l > 0) {
          value[l] = 0;
          floor = atoi(value);
          if (floor < 0) floor = 0;
        }
      }
    }
  }
  if (floor < 0 || n == 0 || n > 16) return -2;
  char link[64], path[64], text[17];
  int pid, used = 0;
  snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
  long l = syscall(SYS_readlinkat, AT_FDCWD, link, path, sizeof(path) - 1);
  if (l <= 0) return -2;
  path[l] = 0;
  sscanf(path, "/proc/%d/oom_score_adj%n", &pid, &used);
  if (used == 0 || path[used] || pid <= 0) return -2;
  memcpy(text, buf, n);
  text[n] = 0;
  char* end;
  long adj = strtol(text, &end, 10);
  if (end == text || (*end && strcmp(end, "\n")) || adj < -1000 || adj > 1000)
    return -2;
  int m = snprintf(text, sizeof(text), "%d", innkeeper_oom_adj(adj, floor));
  return syscall(SYS_write, fd, text, m) < 0 ? -1 : (long)n;
}
static long do_write(int fd, const void* buf, size_t n) {
  long r = oom_write(fd, buf, n);
  if (r == -2) r = psi_write(fd, buf, n);
  return r != -2 ? r : syscall(SYS_write, fd, buf, n);
}
EXPORT ssize_t write(int fd, const void* buf, size_t n) {
  return do_write(fd, buf, n);
}
// (Not through write(): FORTIFY turns that call back into this one.)
EXPORT ssize_t __write_chk(int fd, const void* buf, size_t n, size_t buf_size) {
  if (n > buf_size) abort();
  return do_write(fd, buf, n);
}

// socket(AF_KEY): the sandbox refuses the family before our
// seccomp filter sees the call. The connectivity stack opens and closes one
// only to wait for an RCU grace period after swapping eBPF maps, which
// emulated maps don't need (supervisor/broker/src/bpf.rs).
EXPORT int socket(int domain, int type, int protocol) {
  long r = syscall(SYS_socket, domain, type, protocol);
  if (r < 0 && errno == EAFNOSUPPORT && domain == AF_KEY)
    return eventfd(0, (type & SOCK_CLOEXEC) ? EFD_CLOEXEC : 0);
  return r;
}

// init forks services with "namespace mnt|pid" through clone(); without the
// namespaces they share the session's.
#define CLONE_NEW_ALL                                                        \
  (CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWUTS | CLONE_NEWIPC | \
   CLONE_NEWUSER | CLONE_NEWCGROUP)
EXPORT int clone(int (*fn)(void*), void* stack, int flags, void* arg, ...) {
  static int (*real)(int (*)(void*), void*, int, void*, ...);
  if (!real)
    real = (int (*)(int (*)(void*), void*, int, void*, ...))dlsym(
      RTLD_NEXT, "clone"
    );
  va_list ap;
  va_start(ap, arg);
  pid_t* ptid = va_arg(ap, pid_t*);
  void* tls = va_arg(ap, void*);
  pid_t* ctid = va_arg(ap, pid_t*);
  va_end(ap);
  int r = real(fn, stack, flags, arg, ptid, tls, ctid);
  if (r == -1 && errno == EPERM && (flags & CLONE_NEW_ALL))
    r = real(fn, stack, flags & ~CLONE_NEW_ALL, arg, ptid, tls, ctid);
  return r;
}

// /proc/config.gz: system_server (Debug.isVmapStack, via libvintf's
// LoadKernelConfigs) CHECKs that it can read the kernel config, but distro
// kernels often don't build CONFIG_IKCONFIG_PROC, and procfs can't be given a
// file it lacks. The session launcher stages the host's /boot/config-$(uname
// -r) as /innkeeper/config.gz; serve that when /proc/config.gz is missing.
// (zygote, and so system_server and apps, preload this shim.)
typedef void* gzFile;
EXPORT gzFile gzopen(const char* path, const char* mode) {
  static gzFile (*real)(const char*, const char*);
  if (!real)
    real = (gzFile (*)(const char*, const char*))dlsym(RTLD_NEXT, "gzopen");
  if (!real) return NULL;
  gzFile f = real(path, mode);
  if (!f && path && strcmp(path, "/proc/config.gz") == 0)
    f = real("/innkeeper/config.gz", mode);
  return f;
}

// Quota project ids: vold (and installd) tag app directories with
// FS_IOC_FSSETXATTR and FS_PROJINHERIT_FL. Only the initial user namespace may
// change a project id, so the first fails with EINVAL here, and filesystems
// without project quotas (btrfs) reject the flag. vold forgives that only on
// ext4 ("legacy userdata"); elsewhere it gives up on the app's external dirs,
// getExternalFilesDir() returns null, and apps that use it crash (Godot does
// at startup). Treat the refusal as success, as vold does on ext4, and set the
// other flags without FS_PROJINHERIT_FL.
EXPORT int ioctl(int fd, int request, ...) {
  va_list ap;
  va_start(ap, request);
  void* arg = va_arg(ap, void*);
  va_end(ap);
  long rc = syscall(SYS_ioctl, fd, request, arg);
  if (rc == -1 && (errno == EINVAL || errno == EPERM || errno == EOPNOTSUPP)) {
    if ((unsigned)request == FS_IOC_FSSETXATTR) return 0;
    if (
      (unsigned)request == FS_IOC_SETFLAGS && arg &&
      (*(unsigned*)arg & FS_PROJINHERIT_FL)
    ) {
      unsigned flags = *(unsigned*)arg & ~FS_PROJINHERIT_FL;
      return syscall(SYS_ioctl, fd, request, &flags);
    }
  }
  return rc;
}

// Single-uid credentials (supervisor/README.md): the kernel sees every file
// as owned
// by uid 0, and the seccomp supervisor records the owner a chown asked for in
// the "user.innkeeper.owner" xattr ("uid:gid"). installd verifies the owners of
// the app directories it prepares (fs_prepare_dir_strict, the cache dirs, the
// fts walks of fixupAppData and clearAppCache) and treats a mismatch as a
// failure, which makes PackageManager wipe and recreate the app's data on
// every boot. Report the recorded owner. Only processes whose rc sets
// INNKEEPER_VSTAT=1 (installd) pay for the extra getxattrat per stat. Not
// zygote: its children include sandboxed processes (WebView renderers) whose
// own seccomp policy kills them for a syscall it doesn't allow. Bionic's own
// callers (fts) reach these wrappers through the PLT too.

static int vstat_state = -1;
static int vstat_on(void) {
  if (vstat_state < 0) {
    const char* e = getenv("INNKEEPER_VSTAT");
    vstat_state = e && !strcmp(e, "1");
  }
  return vstat_state;
}

static void vstat_apply(long n, const char* buf, struct stat* st) {
  if (n <= 0 || n >= 32) return;
  char tmp[32];
  memcpy(tmp, buf, n);
  tmp[n] = 0;
  unsigned uid, gid;
  if (sscanf(tmp, "%u:%u", &uid, &gid) == 2) {
    st->st_uid = uid;
    st->st_gid = gid;
  }
}

// getxattrat(2) (Linux 6.13): the session's seccomp filter traps every
// getxattr/lgetxattr (for security.selinux), which would cost a supervisor
// round trip per stat; getxattrat isn't trapped. Older kernels fall back.
#ifndef __NR_getxattrat
#define __NR_getxattrat 464
#endif
struct innkeeper_xattr_args {
  uint64_t value;
  uint32_t size;
  uint32_t flags;
};
static int have_getxattrat = 1;

static long vstat_getxattr(
  int dirfd, const char* path, int flags, char* buf, size_t len
) {
  static const char kName[] = "user.innkeeper.owner";
  if (have_getxattrat) {
    struct innkeeper_xattr_args a = {(uintptr_t)buf, (uint32_t)len, 0};
    long n = syscall(
      __NR_getxattrat, dirfd, path,
      flags & (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH), kName, &a, sizeof(a)
    );
    if (n >= 0 || errno != ENOSYS) return n;
    have_getxattrat = 0;
  }
  if ((flags & AT_EMPTY_PATH) && !*path)
    return syscall(SYS_fgetxattr, dirfd, kName, buf, len);
  char p[4096];
  const char* q = path;
  if (path[0] != '/' && dirfd != AT_FDCWD) {
    if (
      snprintf(p, sizeof(p), "/proc/self/fd/%d/%s", dirfd, path) >=
      (int)sizeof(p)
    )
      return -1;
    q = p;
  }
  return syscall(
    (flags & AT_SYMLINK_NOFOLLOW) ? SYS_lgetxattr : SYS_getxattr, q, kName, buf,
    len
  );
}

static void vstat_fixup(
  int dirfd, const char* path, int flags, struct stat* st
) {
  if (!S_ISDIR(st->st_mode) && !S_ISREG(st->st_mode)) return;
  char buf[32];
  vstat_apply(vstat_getxattr(dirfd, path, flags, buf, sizeof(buf)), buf, st);
}

EXPORT int fstatat(int dirfd, const char* path, struct stat* st, int flags) {
  int saved = errno;
  long r = syscall(SYS_newfstatat, dirfd, path, st, flags);
  // vold prepares its mount points with lstat, and MediaProvider lstats the
  // volume root. These stand for mounts: show the directory, not the link.
  if (
    r == 0 && S_ISLNK(st->st_mode) && (flags & AT_SYMLINK_NOFOLLOW) &&
    (!strcmp(path, "/storage/emulated") || !strcmp(path, kEmulatedMount) ||
     !strcmp(path, "/mnt/pass_through/0/emulated"))
  )
    r = syscall(SYS_newfstatat, dirfd, path, st, flags & ~AT_SYMLINK_NOFOLLOW);
  if (r == 0 && vstat_on()) {
    vstat_fixup(dirfd, path, flags, st);
    errno = saved;
  }
  return r;
}
EXPORT int fstatat64(int dirfd, const char* path, struct stat64* st, int flags)
  __attribute__((alias("fstatat")));
EXPORT int stat(const char* path, struct stat* st) {
  return fstatat(AT_FDCWD, path, st, 0);
}
EXPORT int stat64(const char* path, struct stat64* st)
  __attribute__((alias("stat")));
EXPORT int lstat(const char* path, struct stat* st) {
  return fstatat(AT_FDCWD, path, st, AT_SYMLINK_NOFOLLOW);
}
EXPORT int lstat64(const char* path, struct stat64* st)
  __attribute__((alias("lstat")));
EXPORT int fstat(int fd, struct stat* st) {
  int saved = errno;
  long r = syscall(SYS_fstat, fd, st);
  if (r == 0 && vstat_on()) {
    vstat_fixup(fd, "", AT_EMPTY_PATH, st);
    errno = saved;
  }
  return r;
}
EXPORT int fstat64(int fd, struct stat64* st) __attribute__((alias("fstat")));
