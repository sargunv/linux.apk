# supervisor

The syscall boundary between the host kernel and Android. One seccomp filter,
installed on Android's init and inherited by every Android process, turns what
an unprivileged namespace can't do into user notifications.
`mise run build:supervisor` builds all three artifacts.

| Dir          | Artifact (stage path)                                            | Runs                                                     | Start reading                 |
| ------------ | ---------------------------------------------------------------- | -------------------------------------------------------- | ----------------------------- |
| `pivot-exec` | `libexec/innkeeper/pivot-exec` (static C)                        | starts init under the filter, then becomes the broker    | `pivot-exec.c`                |
| `broker`     | `share/innkeeper/android/system/bin/innkeeper-brk` (bionic Rust) | pivot-exec's process: Android's parent                   | `src/notify.rs`, `src/bpf.rs` |
| `shim`       | `share/innkeeper/android/system/lib64/libinnkeeper_shim.so`      | LD_PRELOADed into init and the services whose rc sets it | `innkeeper_shim.c`            |

## What is trapped, and who serves it

| Syscalls                                                                                                                                      | Served by       | Why                                                                    |
| --------------------------------------------------------------------------------------------------------------------------------------------- | --------------- | ---------------------------------------------------------------------- |
| `openat` of `/dev/{binder,hwbinder,vndbinder}`, `ioctl` type `'b'`                                                                            | broker (binder) | binder in userspace: no binderfs, no SELinux module                    |
| `bpf`, `statfs` of `/sys/fs/bpf`, BPF-pin `getxattr`, `SO_ANDROID_DROP_REASON`, `RLIMIT_MEMLOCK`, alarm `timerfd_create`, RT scheduling       | broker (`bpf`)  | netd and the BPF loaders need eBPF; the rest fails in a user namespace |
| `get*id`/`set*id`, groups, the `chown` family, `SO_PEERCRED`, `lstat` of owner-tracked paths, `faccessat(W_OK)`, opening `/proc/<pid>/status` | broker (creds)  | virtual Android uids without a subordinate id range                    |
| `readlink`/`readlinkat`, `getcwd`, `fstat`, `mkdir`/`mkdirat`, the `chmod` family; `openat` of faked `/proc` files                            | broker (`fs`)   | no mounts or user namespace in the sandbox (docs/session.md)           |
| `capget`, `capset`, the capability `prctl`s, `setsockopt(SO_MARK)`                                                                            | broker (`bpf`)  | no capabilities in the sandbox; the host's network                     |
| `openat` and `inotify_add_watch` of `/dev/input`, `ioctl` type `'E'`                                                                          | broker (input)  | session-local evdev devices the bridge feeds, never the host's         |

pivot-exec is static C because it runs before Android's linker exists, and
defines no syscall policy. Its child execs `innkeeper-brk --init`, which
installs the filter (`src/filter.rs`), hands the listener back and execs init.
pivot-exec then execs the broker in its own process, which serves every
notification, reaps orphans as the subreaper and exits with init's status. It
stays an ancestor of every Android process, so process-memory access works with
Yama's default `ptrace_scope=1` without capabilities or a sysctl override.

Filesystem notifications (`fs`) run on four workers so a slow lookup (including
`fstat` through a caller's fd) cannot block the binder receive thread. Each BPF
object retains one memfd inode; opening a pin reopens that inode with an
independent file description, so netd's OFD map locks conflict correctly.

## Binder

`src/binder/driver.rs` follows `drivers/android/binder.c` and makes no syscalls,
so it is unit-tested on the host; `src/notify.rs` binds it to real processes:

- binder `openat` gets a 4 MiB memfd injected (`SECCOMP_IOCTL_NOTIF_ADDFD`),
  which the process maps as its receive area; the broker writes through its own
  mapping;
- ioctls reach the caller's memory through `/proc/<pid>/mem`, opened at binder
  open, which pins its address space;
- a read with nothing to do leaves its notification unanswered until there is
  work, and stays interruptible, so ptrace and job control work;
- polling threads get an eventfd in place of their binder fd;
- `sender_euid` is the opener's virtual euid;
- the freezer works as in the kernel, so ActivityManager freezes cached apps
  (`dumpsys activity` shows `use_freezer=true`): a `BINDER_FREEZE` that waits
  for its target to drain is answered when it drains or at its timeout
  (`notify.rs`'s monitor thread runs the timer). There is no cgroup freezer in
  the sandbox. `/dev/binderfs/features` (made by the launcher) tells libbinder
  the broker has freeze notifications, extended errors and oneway spam
  detection.

Not supported: priority inheritance, binder netlink reports and `TF_UPDATE_TXN`
(Android 17 doesn't send it). Two deviations from the kernel:

- `ADDFD` can't be undone, so if a transaction's later fd doesn't fit the
  reader's fd table, the fds already installed stay, and leak if the transaction
  then fails for good.
- A looper whose wait a signal ended still counts as idle until work wakes it
  and finds it gone (checking every waiter would cost a syscall per read). That
  delays a `BR_SPAWN_LOOPER` at most until the thread enters the ioctl again;
  only a cycle of nested callbacks that needs a new looper waits for its
  restart.

One broker thread serves binder for the whole session, about 70k to 90k
transactions per second; a round trip costs 15 µs against 3.8 µs for kernel
binder.

## Single-uid credentials

`src/creds/table.rs` is the pure model, `src/creds/syscalls.rs` the syscall
side. A process's identity is an id in its soft `RLIMIT_RTTIME`, which `fork`
inherits and `execve` keeps, so there is no fork tracking; the session has no
real-time tasks for the limit to affect. `set*id` follow the kernel's rules
applied to the virtual ids, and capabilities are virtual.

**The owner contract.** A `chown` runs as a no-op `fchownat(-1, -1)` (for the
kernel's errors and side effects) and records the owner as
`user.innkeeper.owner` = `"uid:gid"`. Readers:

- the broker's emulated `lstat` and `faccessat(W_OK)` for `/data`, the BPF pins,
  `/metadata` and clatd (a file without a record inherits its closest recorded
  ancestor's owner, so ART sees an app's APK as not writable by it);
- the shim's `stat` family in installd (`INNKEEPER_VSTAT=1` in its rc);
- `image/derive.sh single`, which writes the same xattr on image files whose
  owner Android verifies.

Not virtualized: `SCM_CREDENTIALS` (logd sees every writer as uid 0), owners of
files created without a `chown`, and vold's `/proc` scan.

## The shim

Hidden SELinux breaks call sites in init and a few services that assume a policy
(`security_compute_create`, `set*con`, `selinux_status_*`); the shim turns them
into no-ops. It also makes priority and rlimit changes best-effort, treats
vold's refused quota-project ioctl as success, skips POSIX ACL xattrs, and
serves the staged `/proc/config.gz`.

The Flatpak sandbox's own seccomp filter refuses some calls before ours sees
them, so the shim covers them where it is loaded: `mount`, `umount`, `unshare`,
`setns` and namespaced `clone` succeed as no-ops; `socket(AF_KEY)` (which the
connectivity stack only opens and closes) gets an eventfd; netd's refused
rtnetlink requests read as done (`INNKEEPER_NETLINK_ACCEPT`); vold's uevent
socket binds to a kernel-chosen port (concurrent sessions share the host's
network, so their pid-numbered ports collide); and lmkd's PSI trigger windows
are widened to 2 s (`INNKEEPER_PSI_WINDOW`). Init isn't pid 1 in the sandbox, so
pivot-exec marks it with `INNKEEPER_SHIM_INIT`.

lmkd also opts into `INNKEEPER_OOM_ADJ`: its writes to
`/proc/<pid>/oom_score_adj` map Android's -1000…1000 range into the inherited
session score…1000 (with a minimum floor of 0). The kernel lets an unprivileged
process lower a score back to that floor, so a cached app returning to the
foreground regains its relative protection. lmkd keeps the original Android
scores for its own kill policy.

Android 17 mounts every visible emulated volume through FUSE, but Flatpak has no
`/dev/fuse` and cannot mount filesystems. For vold only
(`INNKEEPER_EMULATED_STORAGE=1`), `shim/fuse.h` provides a socket with the
stable FUSE INIT request so MediaProvider starts its storage session. The
launcher links `/storage/emulated`, `/mnt/user/0/emulated` and
`/mnt/pass_through/0/emulated` to `/data/media`. All file access uses that same
filesystem, with no FUSE cache or filesystem requests. MediaProvider receives
the mounted-volume notification, scans the volume and serves MediaStore; apps
can index later file writes with MediaScanner or create them through MediaStore.
Unmount closes the socket and ends the session. This supports the primary
emulated volume, not AppFuse proxy descriptors or removable volumes.
Canonicalization keeps the public path, while cross-process `/proc` reads keep
the lower path so vold does not kill MediaProvider's recovery database on
unmount.

## OpenHarmony

`INNKEEPER_GUEST=ohos` ([ohos/](../ohos/README.md)) keeps everything above and
adds what OpenHarmony's userspace expects of its kernel. Android sessions never
set it, and their filter and driver are unchanged.

- **musl.** The broker is also built static for `x86_64-unknown-linux-musl`
  (`mise run ohos:build`), since OH's root has no Bionic. musl opens with
  `open(2)`, so the filter also traps it and the broker serves it as
  `openat(AT_FDCWD, ...)`. `/etc/passwd` and `/etc/group` get `/system/etc`'s:
  musl resolves service users there, and Flatpak binds its own over both.
- **Binder.** OH's kernel adds `BINDER_FEATURE_SET` (access tokens and sender
  info), `BINDER_GET_ACCESS_TOKEN` and `BINDER_GET_SENDER_INFO`. A transaction
  carries its sender's token and first token, and the reading thread keeps them,
  with the sender's pid for synchronous calls, until its next read.
- **Access tokens** (`src/ohos.rs`). `/dev/access_token_id` is an empty file
  whose type-`'A'` ioctls the broker serves: each process's token and first
  token, set by init or appspawn before exec and inherited across fork, and the
  kernel's per-token permission maps, which init fills from the service cfgs and
  the access-token service checks. Records are keyed by pid and start time, so a
  reused pid starts from its parent's. Newer kernels' SPM entries and reference
  counts are absent (`ENOTTY`), as on a kernel built without them.
- **Ashmem** (`src/ashmem.rs`). OH's c_utils and everything on it (IPC parcels,
  the display HDI's command queues) need `/dev/ashmem`. Each open gets a fresh
  sealable memfd named `ashmem`, which the broker's fstat shows as a character
  device (c_utils checks), and the broker serves ashmem's ioctls on it: the size
  is the memfd's, a mask without `PROT_WRITE` is `F_SEAL_FUTURE_WRITE`, and
  pinning and purging do nothing (memfd pages are never purged). The state lives
  in the memfd, so a region passed to another process answers the same there.

## Tests

`mise run test` runs `cargo test -p innkeeper-broker` (binder transactions,
failure delivery, freezing, credentials) and every `tests/*.sh`: C probes that
drive the real filter and broker (`seccomp-filter` checks exactly which syscalls
notify, `bpfemu` the emulator's bounds, `binder-restarts` interrupted ioctls,
`drm-node` the render-node policy, `bootstrap` pivot-exec), and unit checks of
the shim's headers.

- `mise run test:e2e -- boot` boots and runs `selftest`.
- `mise run ohos:test` boots OpenHarmony until its system abilities register.

How the supervisor fits the session: [docs/session.md](../docs/session.md); what
it may and may not defend against: [docs/security.md](../docs/security.md).
