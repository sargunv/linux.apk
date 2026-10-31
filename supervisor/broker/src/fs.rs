//! The session runs in the Flatpak sandbox's own namespaces, with no mounts.
//! Android's root is the sandbox's `/`, where
//! `/system`, `/apex`, `/data`... are symlinks into a composed tree on disk
//! (`host/launcher/src/flatpak.rs`). The broker covers what the mounts did:
//!
//!  * `readlink` of `/proc` magic links (`/proc/self/fd/N`, `exe`, `cwd`) and
//!    `getcwd` name the host path behind a symlink, which the linker and
//!    bionic's `realpath` would take as the canonical one (and then refuse a
//!    library outside its namespace's `/system/...` paths). Paths under a
//!    mapped host dir are shown as their Android path.
//!  * `fstat` of a file the user owns reports owner root: there is no
//!    user namespace mapping the user to Android's root, and bionic only
//!    trusts root-owned property areas (as in a single-uid session, the
//!    files are all the user's anyway).
//!  * SO_PEERCRED shows init as pid 1 (see [`init_as_pid1`]).
//!  * lstat of a symlink that stands in for a bind mount inside /data
//!    (/data/user/0 -> /data/data) shows the directory (see [`bind_target`]).
//!  * `mkdir` and the `chmod` family keep a directory's owner rwx: with no
//!    capabilities, Android's root can't write into a 0511 directory such as
//!    `/data/user` that it made itself. (Files keep their mode: code checks
//!    exact modes, such as ClatCoordinator's 0440 eBPF pins.)
//!    The broker runs these itself, so a path through `/proc/self`, directly
//!    or by a symlink (OpenHarmony's per-app `/data/storage`), names the
//!    caller's.
//!  * `openat` of a `/proc` file the session fakes (`/innkeeper/proc/<path>`,
//!    which bind mounts covered otherwise) gets a memfd copy of it, and
//!    apexd's `/proc/mounts` lists the pre-mounted APEXes
//!    (`/innkeeper/apexd-mounts`, as `innkeeper-apexd` does with a bind mount).
//!  * OpenHarmony only: `openat` of `/etc/passwd` and `/etc/group` gets the
//!    image's (`/system/etc`, its `/etc`): musl resolves service users by
//!    name there, and the sandbox binds its own read-only over both.

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::notify::{
    SECCOMP_ADDFD_FLAG_SEND, SeccompNotif, SeccompNotifAddfd, mem_write, notif_addfd_cmd,
    notif_id_valid, open_mem, read_path, send_resp, status_field, vm_read,
};

pub(crate) struct Fs {
    listener: RawFd,
    resp_sz: usize,
    /// (host dir, Android path), longest host dir first.
    map: Vec<(Vec<u8>, Vec<u8>)>,
    /// The kernel's uid and gid for every Android process (the user's).
    uid: u32,
    gid: u32,
    /// Set by the launcher, including an empty path for software rendering.
    render_node: Option<std::path::PathBuf>,
}

/// Filesystem lookups can wait on a mount. Keep them off the binder receive
/// thread, including fstat's traversal of /proc/<tid>/fd/<fd>.
pub(crate) struct Workers(std::sync::mpsc::Sender<SeccompNotif>);

impl Workers {
    pub(crate) fn new(listener: RawFd, resp_sz: usize) -> Self {
        use std::sync::{Arc, Mutex, mpsc};
        let fs = Arc::new(Fs::from_env(listener, resp_sz));
        let (tx, rx) = mpsc::channel::<SeccompNotif>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..4 {
            let fs = Arc::clone(&fs);
            let rx = Arc::clone(&rx);
            std::thread::spawn(move || {
                loop {
                    let Ok(req) = rx.lock().unwrap().recv() else {
                        return;
                    };
                    // Queued notifications can have ended before we start.
                    if !notif_id_valid(listener, req.id) {
                        continue;
                    }
                    if req.data.nr as i64 == libc::SYS_openat {
                        if !fs.openat(&req) {
                            fs.cont(req.id);
                        }
                    } else {
                        fs.serve(&req);
                    }
                }
            });
        }
        Self(tx)
    }

    pub(crate) fn submit(&self, req: SeccompNotif) {
        self.0.send(req).expect("fs workers stopped");
    }
}

const SYS_FCHMODAT2: i64 = 452;

/// The syscalls [`Fs::serve`] answers.
pub(crate) fn is_fs_syscall(nr: i64) -> bool {
    matches!(
        nr,
        libc::SYS_readlinkat
            | libc::SYS_readlink
            | libc::SYS_getcwd
            | libc::SYS_fstat
            | libc::SYS_mkdir
            | libc::SYS_mkdirat
            | libc::SYS_chmod
            | libc::SYS_fchmod
            | libc::SYS_fchmodat
            | SYS_FCHMODAT2
    )
}

/// The path map, for the credential side (`creds`): Android
/// paths of mapped host dirs (such as `/data`) as host paths, so lookups
/// that refuse symlinks can start past them.
pub(crate) fn to_host(p: &[u8]) -> Option<Vec<u8>> {
    static F: std::sync::OnceLock<Fs> = std::sync::OnceLock::new();
    let f = F.get_or_init(|| Fs::from_env(-1, 0));
    for (host, android) in &f.map {
        if android.as_slice() == b"/" {
            // The composed root: "/" has a symlink to it per partition
            // (/system, /apex...).
            let first = p.get(1..)?.split(|&c| c == b'/').next()?;
            let mut target = host.clone();
            target.push(b'/');
            target.extend_from_slice(first);
            let mut link = b"/".to_vec();
            link.extend_from_slice(first);
            if first.is_empty()
                || std::fs::read_link(OsStr::from_bytes(&link))
                    .map_or(true, |t| t.as_os_str().as_bytes() != target.as_slice())
            {
                continue;
            }
            let mut out = host.clone();
            out.extend_from_slice(p);
            return Some(out);
        }
        let Some(rest) = p.strip_prefix(android.as_slice()) else {
            continue;
        };
        if rest.is_empty() || rest[0] == b'/' {
            let mut out = host.clone();
            out.extend_from_slice(rest);
            return Some(out);
        }
    }
    None
}

/// DRM enumeration may read directories, but only the chosen device opens.
fn drm_path_allowed(path: &std::path::Path, selected: &std::path::Path) -> bool {
    !path.starts_with("/dev/dri") || path == selected || path.is_dir()
}

/// Init isn't pid 1, but its property service treats a peer
/// that isn't pid 1 as a client (and init's own setprop from its restorecon
/// thread as a new restorecon request, forever). Show init as pid 1 to
/// SO_PEERCRED; pivot-exec passes its pid in `INNKEEPER_INIT_PID`.
pub(crate) fn init_as_pid1(pid: i32) -> i32 {
    static INIT: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    let init = INIT.get_or_init(|| std::env::var("INNKEEPER_INIT_PID").ok()?.parse().ok());
    if *init == Some(pid) { 1 } else { pid }
}

/// The Android path a symlink (O_PATH fd) that stands in
/// for a bind mount inside /data points to (see host/launcher/src/flatpak.rs).
pub(crate) fn bind_target(link: &impl AsRawFd) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::readlinkat(
            link.as_raw_fd(),
            c"".as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
        )
    };
    let t = buf.get(..usize::try_from(n).ok()?)?;
    (t.starts_with(b"/data/") && !t.split(|&c| c == b'/').any(|c| c == b"..")).then(|| t.to_vec())
}

/// `st`'s owner as Android sees it: the user's ids are
/// root's (see [`Fs::fstat`]).
pub(crate) fn root_ids(st: &mut libc::stat) {
    let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
    if st.st_uid == u {
        st.st_uid = 0;
    }
    if st.st_gid == g {
        st.st_gid = 0;
    }
}

impl Fs {
    /// From `INNKEEPER_ROOT_MAP` (lines of
    /// `<host dir>=<Android path>`).
    pub(crate) fn from_env(listener: RawFd, resp_sz: usize) -> Fs {
        let spec = std::env::var("INNKEEPER_ROOT_MAP").unwrap_or_default();
        let mut map: Vec<(Vec<u8>, Vec<u8>)> = spec
            .lines()
            .filter_map(|l| l.rsplit_once('='))
            .map(|(h, a)| {
                (
                    h.trim_end_matches('/').as_bytes().to_vec(),
                    a.as_bytes().to_vec(),
                )
            })
            .filter(|(h, _)| !h.is_empty())
            .collect();
        map.sort_by_key(|(h, _)| std::cmp::Reverse(h.len()));
        Fs {
            listener,
            resp_sz,
            render_node: std::env::var_os("INNKEEPER_RENDER_NODE").map(Into::into),
            map,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// `p` with a mapped host dir replaced by its Android path.
    fn to_android(&self, p: &[u8], public_storage: bool) -> Vec<u8> {
        for (host, android) in &self.map {
            // A realpath/getcwd caller needs the public volume name. A scan
            // of another process's fds (vold's unmount cleanup) needs the
            // lower path, or it kills MediaProvider's open recovery database.
            if !public_storage && android == b"/storage/emulated" {
                continue;
            }
            let Some(rest) = p.strip_prefix(host.as_slice()) else {
                continue;
            };
            if !rest.is_empty() && rest[0] != b'/' {
                continue;
            }
            let mut out = android.clone();
            if out.last() == Some(&b'/') && !rest.is_empty() {
                out.pop();
            }
            out.extend_from_slice(rest);
            if out.is_empty() {
                out.push(b'/');
            }
            return out;
        }
        p.to_vec()
    }

    fn reply(&self, id: u64, val: i64, errno: i32) {
        send_resp(self.listener, self.resp_sz, id, val, -errno, false);
    }

    fn cont(&self, id: u64) {
        send_resp(self.listener, self.resp_sz, id, 0, 0, true);
    }

    /// Answer readlink/readlinkat/getcwd/fstat.
    pub(crate) fn serve(&self, req: &SeccompNotif) {
        let tid = req.pid as i32;
        let a = req.data.args;
        let nr = req.data.nr as i64;
        match nr {
            libc::SYS_fstat => return self.fstat(req),
            libc::SYS_mkdir | libc::SYS_mkdirat => return self.mkdir(req),
            libc::SYS_chmod | libc::SYS_fchmod | libc::SYS_fchmodat | SYS_FCHMODAT2 => {
                return self.chmod(req);
            }
            _ => {}
        }
        let (link, buf, size, public_storage) = if nr == libc::SYS_getcwd {
            (
                format!("/proc/{tid}/cwd").into_bytes(),
                a[0],
                a[1] as usize,
                true,
            )
        } else {
            let (path, buf, size) = if nr == libc::SYS_readlink {
                (a[0], a[1], a[2] as usize)
            } else {
                // Only absolute paths: the /proc magic links are named so.
                (a[1], a[2], a[3] as usize)
            };
            match read_path(tid, path).and_then(|p| proc_path(tid, &p)) {
                Some((p, own)) => (p, buf, size, own),
                None => return self.cont(req.id),
            }
        };
        let target = match std::fs::read_link(OsStr::from_bytes(&link)) {
            Ok(t) => t,
            // E.g. a non-dumpable process's fds: it can read its own.
            Err(_) => return self.cont(req.id),
        };
        let mut out = self.to_android(target.as_os_str().as_bytes(), public_storage);
        let n = if nr == libc::SYS_getcwd {
            out.push(0);
            if out.len() > size {
                return self.reply(req.id, 0, libc::ERANGE);
            }
            out.len()
        } else {
            out.len().min(size)
        };
        let Some(mem) = open_mem(tid) else {
            return self.cont(req.id);
        };
        if !notif_id_valid(self.listener, req.id) {
            return;
        }
        if !mem_write(mem.raw(), buf, &out[..n]) {
            return self.reply(req.id, 0, libc::EFAULT);
        }
        self.reply(req.id, n as i64, 0);
    }

    /// fstat(fd, st) through the caller's /proc fd link, with the user's ids
    /// shown as root's.
    fn fstat(&self, req: &SeccompNotif) {
        let tid = req.pid as i32;
        let (fd, buf) = (req.data.args[0] as i32, req.data.args[1]);
        let Ok(link) = std::ffi::CString::new(format!("/proc/{tid}/fd/{fd}")) else {
            return self.cont(req.id);
        };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if fd < 0
            || unsafe { libc::stat(link.as_ptr(), &mut st) } != 0
            || (st.st_uid != self.uid && st.st_gid != self.gid)
        {
            return self.cont(req.id);
        }
        if st.st_uid == self.uid {
            st.st_uid = 0;
        }
        if st.st_gid == self.gid {
            st.st_gid = 0;
        }
        // OpenHarmony's c_utils takes /dev/ashmem only as a character device.
        if crate::guest() == crate::Guest::Ohos
            && std::fs::read_link(OsStr::from_bytes(link.as_bytes()))
                .is_ok_and(|l| crate::ashmem::is_region(l.as_os_str().as_bytes()))
        {
            st.st_mode = libc::S_IFCHR | 0o666;
            st.st_rdev = crate::ashmem::RDEV;
        }
        let b = unsafe {
            std::slice::from_raw_parts(
                &st as *const libc::stat as *const u8,
                size_of::<libc::stat>(),
            )
        };
        let Some(mem) = open_mem(tid) else {
            return self.cont(req.id);
        };
        if !notif_id_valid(self.listener, req.id) {
            return;
        }
        if !mem_write(mem.raw(), buf, b) {
            return self.reply(req.id, 0, libc::EFAULT);
        }
        self.reply(req.id, 0, 0);
    }

    /// The host path a path argument names for task `tid` (`dirfd` for
    /// relative ones, AT_FDCWD for its cwd); None to let the kernel run it.
    fn target_path(&self, tid: i32, dirfd: i32, addr: u64) -> Option<std::ffi::CString> {
        let p = read_path(tid, addr)?;
        if p.is_empty() {
            return None;
        }
        let full = if p[0] == b'/' {
            p
        } else {
            let link = if dirfd == libc::AT_FDCWD {
                format!("/proc/{tid}/cwd")
            } else {
                format!("/proc/{tid}/fd/{dirfd}")
            };
            let mut d = std::fs::read_link(link).ok()?.into_os_string().into_vec();
            d.push(b'/');
            d.extend_from_slice(&p);
            d
        };
        // Android paths through /proc/self must name the caller, never our
        // own fds (which include the retained bpffs directory).
        let full = if full.starts_with(b"/proc/") {
            proc_path(tid, &full)?.0
        } else {
            full
        };
        std::ffi::CString::new(caller_links(tid, full)?).ok()
    }

    /// mkdir/mkdirat with the owner's rwx kept (the caller's umask applies).
    fn mkdir(&self, req: &SeccompNotif) {
        let tid = req.pid as i32;
        let a = req.data.args;
        let (dirfd, path, mode) = if req.data.nr as i64 == libc::SYS_mkdir {
            (libc::AT_FDCWD, a[0], a[1] as u32)
        } else {
            (a[0] as i32, a[1], a[2] as u32)
        };
        let Some(p) = self.target_path(tid, dirfd, path) else {
            return self.cont(req.id);
        };
        let umask = status_field(tid, "Umask:")
            .and_then(|u| u32::from_str_radix(&u, 8).ok())
            .unwrap_or(0o022);
        let mode = (mode & 0o7777 & !umask) | 0o700;
        if !notif_id_valid(self.listener, req.id) {
            return;
        }
        // The broker's own umask is 0 (main()).
        let r = unsafe { libc::mkdir(p.as_ptr(), mode) };
        self.answer(req.id, r);
    }

    /// The chmod family, with a directory's owner rwx kept.
    fn chmod(&self, req: &SeccompNotif) {
        let tid = req.pid as i32;
        let a = req.data.args;
        let nr = req.data.nr as i64;
        let (p, mode, nofollow) = match nr {
            libc::SYS_fchmod => {
                match std::ffi::CString::new(format!("/proc/{tid}/fd/{}", a[0] as i32)) {
                    Ok(p) => (Some(p), a[1] as u32, false),
                    Err(_) => (None, 0, false),
                }
            }
            libc::SYS_chmod => (
                self.target_path(tid, libc::AT_FDCWD, a[0]),
                a[1] as u32,
                false,
            ),
            _ => (
                self.target_path(tid, a[0] as i32, a[1]),
                a[2] as u32,
                nr == SYS_FCHMODAT2 && a[3] & libc::AT_SYMLINK_NOFOLLOW as u64 != 0,
            ),
        };
        let Some(p) = p else {
            return self.cont(req.id);
        };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = if nofollow {
            unsafe { libc::lstat(p.as_ptr(), &mut st) }
        } else {
            unsafe { libc::stat(p.as_ptr(), &mut st) }
        };
        if r != 0 || st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return self.cont(req.id);
        }
        if !notif_id_valid(self.listener, req.id) {
            return;
        }
        let r = unsafe { libc::chmod(p.as_ptr(), (mode & 0o7777) | 0o700) };
        self.answer(req.id, r);
    }

    /// Answer with a libc return value (0, or -1 and errno).
    fn answer(&self, id: u64, r: libc::c_int) {
        if r == 0 {
            self.reply(id, 0, 0);
        } else {
            self.reply(
                id,
                0,
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            );
        }
    }

    /// Serve an `openat` of a faked /proc file; false if it isn't one.
    pub(crate) fn openat(&self, req: &SeccompNotif) -> bool {
        let tid = req.pid as i32;
        let a = req.data.args;
        // Flatpak grants DRM to the bridge as well. Android may only open
        // the selected render node: Mesa (including the composer's ANGLE)
        // cannot pick a different GPU from the allocator's consumers.
        let n = (4096 - (a[1] as usize & 4095)).min(6);
        let prefix = vm_read(tid, a[1], n);
        if let Some(selected) = &self.render_node
            && prefix
                .as_ref()
                .is_some_and(|p| !p.starts_with(b"/") || p.starts_with(b"/dev/") || p.len() < 6)
            && let Some(path) = self.target_path(tid, a[0] as i32, a[1])
            && let Ok(path) = std::fs::canonicalize(OsStr::from_bytes(path.to_bytes()))
            && !drm_path_allowed(&path, selected)
        {
            self.reply(req.id, 0, libc::EACCES);
            return true;
        }
        if crate::guest() == crate::Guest::Ohos
            && prefix.as_deref().is_some_and(|p| p.starts_with(b"/dev/"))
            && read_path(tid, a[1]).as_deref() == Some(crate::ashmem::NODE)
        {
            match crate::ashmem::create() {
                Ok(region) => self.inject_fd(req, region.as_raw_fd()),
                Err(_) => self.reply(req.id, 0, libc::ENOMEM),
            }
            return true;
        }
        if crate::guest() == crate::Guest::Ohos
            && prefix.as_deref().is_some_and(|p| p.starts_with(b"/etc/"))
            && let Some(name) = read_path(tid, a[1]).as_deref().and_then(ohos_etc)
        {
            return match std::fs::read(format!("/system/etc/{name}")) {
                Ok(body) => {
                    self.inject(req, &body);
                    true
                }
                Err(_) => false,
            };
        }
        match prefix {
            Some(b) if b"/proc/".starts_with(&b) || b == b"/proc/" => {}
            _ => return false,
        }
        let Some(path) = read_path(tid, a[1]) else {
            return false;
        };
        if path.split(|&c| c == b'/').any(|c| c == b"..") {
            return false;
        }
        // apexd's mount table (init watches its own with epoll, which a memfd
        // can't serve).
        let body = if (path == b"/proc/mounts" || path == b"/proc/self/mounts")
            && std::fs::read(format!("/proc/{tid}/comm")).is_ok_and(|c| c == b"apexd\n")
        {
            let (Ok(mut m), Ok(apex)) = (
                std::fs::read(format!("/proc/{tid}/mounts")),
                std::fs::read("/innkeeper/apexd-mounts"),
            ) else {
                return false;
            };
            m.extend_from_slice(&apex);
            m
        } else {
            let rest = &path[b"/proc/".len()..];
            if rest.is_empty() || rest.split(|&c| c == b'/').any(|c| c.is_empty()) {
                return false;
            }
            let mut fake = b"/innkeeper/proc/".to_vec();
            fake.extend_from_slice(rest);
            match std::fs::read(OsStr::from_bytes(&fake)) {
                Ok(b) => b,
                Err(_) => return false,
            }
        };
        self.inject(req, &body);
        true
    }

    /// Complete `req` (an openat) with a memfd holding `body`.
    fn inject(&self, req: &SeccompNotif, body: &[u8]) {
        let fd =
            unsafe { libc::syscall(libc::SYS_memfd_create, c"proc".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return self.reply(req.id, 0, libc::ENOMEM);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if unsafe { libc::write(fd.as_raw_fd(), body.as_ptr() as *const _, body.len()) }
            != body.len() as isize
            || unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) } != 0
        {
            return self.reply(req.id, 0, libc::EIO);
        }
        self.inject_fd(req, fd.as_raw_fd());
    }

    /// Complete `req` (an openat) with a copy of `fd`.
    fn inject_fd(&self, req: &SeccompNotif, fd: RawFd) {
        let add = SeccompNotifAddfd {
            id: req.id,
            flags: SECCOMP_ADDFD_FLAG_SEND,
            srcfd: fd as u32,
            newfd: 0,
            newfd_flags: (req.data.args[2] as u32) & libc::O_CLOEXEC as u32,
        };
        unsafe {
            libc::ioctl(
                self.listener,
                notif_addfd_cmd(size_of::<SeccompNotifAddfd>()) as _,
                &add as *const _,
            )
        };
    }
}

/// The OpenHarmony `/etc` files the broker serves from `/system/etc`.
fn ohos_etc(path: &[u8]) -> Option<&'static str> {
    match path {
        b"/etc/passwd" => Some("passwd"),
        b"/etc/group" => Some("group"),
        _ => None,
    }
}

/// `path` with each directory on the way that is a symlink through
/// `/proc/self` (OpenHarmony's per-app `/data/storage`, ohos/sandbox.sh)
/// replaced by the caller's: the kernel would resolve it as the broker's.
fn caller_links(tid: i32, mut path: Vec<u8>) -> Option<Vec<u8>> {
    let (mut end, mut hops) = (0, 0);
    while let Some(i) = path.get(end + 1..)?.iter().position(|&c| c == b'/') {
        end += 1 + i;
        let Ok(target) = std::fs::read_link(OsStr::from_bytes(&path[..end])) else {
            continue;
        };
        let Some((mut next, true)) = proc_path(tid, target.as_os_str().as_bytes()) else {
            continue;
        };
        hops += 1;
        if hops > 8 {
            return None;
        }
        next.extend_from_slice(&path[end..]);
        (path, end) = (next, 0);
    }
    Some(path)
}

/// `/proc/self/...` and `/proc/thread-self/...` as the broker must name them
/// for task `tid`, plus whether the request names the caller; None outside /proc.
fn proc_path(tid: i32, p: &[u8]) -> Option<(Vec<u8>, bool)> {
    let rest = p.strip_prefix(b"/proc/")?;
    let tgid = || status_field(tid, "Tgid:");
    let mut out = b"/proc/".to_vec();
    let word = |w: &[u8]| {
        rest.strip_prefix(w)
            .filter(|r| r.is_empty() || r[0] == b'/')
            .map(|r| r.to_vec())
    };
    let own = if let Some(r) = word(b"self") {
        out.extend_from_slice(tgid()?.as_bytes());
        out.extend_from_slice(&r);
        true
    } else if let Some(r) = word(b"thread-self") {
        out.extend_from_slice(format!("{}/task/{tid}", tgid()?).as_bytes());
        out.extend_from_slice(&r);
        true
    } else {
        out.extend_from_slice(rest);
        false
    };
    Some((out, own))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(map: &str) -> Fs {
        unsafe { std::env::set_var("INNKEEPER_ROOT_MAP", map) };
        Fs::from_env(-1, 0)
    }

    #[test]
    fn chmod_path_names_the_callers_proc_fds() {
        let f = Fs {
            listener: -1,
            resp_sz: 0,
            map: Vec::new(),
            uid: 0,
            gid: 0,
            render_node: None,
        };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        // The syscall caller's fd namespace must survive path resolution,
        // even when the supervisor keeps a directory at the same fd number.
        let path = c"/proc/self/fd/3";
        let resolved = f
            .target_path(tid, libc::AT_FDCWD, path.as_ptr() as u64)
            .unwrap();
        assert_eq!(
            resolved.to_bytes(),
            format!("/proc/{}/fd/3", std::process::id()).as_bytes()
        );
        let path = c"/proc/thread-self/fd/3";
        let resolved = f
            .target_path(tid, libc::AT_FDCWD, path.as_ptr() as u64)
            .unwrap();
        assert_eq!(
            resolved.to_bytes(),
            format!("/proc/{}/task/{tid}/fd/3", std::process::id()).as_bytes()
        );
    }

    #[test]
    fn paths_through_a_proc_self_link_name_the_callers() {
        let f = Fs {
            listener: -1,
            resp_sz: 0,
            map: Vec::new(),
            uid: 0,
            gid: 0,
            render_node: None,
        };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        // OpenHarmony's /data/storage -> /proc/self/fd/990: the kernel would
        // follow the link in the broker.
        let dir = std::env::temp_dir().join(format!("innkeeper-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("storage");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/proc/self/fd/3", &link).unwrap();
        let path = std::ffi::CString::new(link.join("el1/database").into_os_string().into_vec());
        let path = path.unwrap();
        let resolved = f.target_path(tid, libc::AT_FDCWD, path.as_ptr() as u64);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            resolved.unwrap().to_bytes(),
            format!("/proc/{}/fd/3/el1/database", std::process::id()).as_bytes()
        );
    }

    #[test]
    fn maps_host_dirs_to_android_paths() {
        let f = fs("/var/r=/\n/var/d/data-x=/data\n/var/d/data-x/media=/storage/emulated");
        assert_eq!(
            f.to_android(b"/var/r/system/lib64/libc.so", true),
            b"/system/lib64/libc.so"
        );
        assert_eq!(f.to_android(b"/var/r", true), b"/");
        assert_eq!(
            f.to_android(b"/var/d/data-x/app/x.apk", true),
            b"/data/app/x.apk"
        );
        assert_eq!(f.to_android(b"/var/d/data-x", true), b"/data");
        assert_eq!(f.to_android(b"/var/rr/x", true), b"/var/rr/x");
        assert_eq!(f.to_android(b"socket:[1]", true), b"socket:[1]");
        assert_eq!(
            f.to_android(b"/var/d/data-x/media/0/a.png", true),
            b"/storage/emulated/0/a.png"
        );
        assert_eq!(
            f.to_android(b"/var/d/data-x/media/0/a.png", false),
            b"/data/media/0/a.png"
        );
    }
}
