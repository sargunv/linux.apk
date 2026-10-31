//! The session's syscall boundary, shared by init bootstrap and host probes.
//! Each rule reloads the syscall number and either notifies or falls through;
//! argument comparisons use the low word, as in the x86_64 syscall ABI.

#[repr(C)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}
#[repr(C)]
struct Program {
    len: u16,
    filter: *const Insn,
}
const NOTIFY: u32 = 0x7fc00000;
const ALLOW: u32 = 0x7fff0000;
pub(crate) const SO_ANDROID_DROP_REASON: u32 = 0x0ad01d01;

fn insn(code: u16, k: u32) -> Insn {
    Insn {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

// (argument index, mask, value). Full masks are equality comparisons;
// W_OK and binder's ioctl type use masked comparisons.
fn rule(f: &mut Vec<Insn>, nr: i64, args: &[(u32, u32, u32)]) {
    f.push(insn(0x20, 0)); // LD W ABS nr
    let first = f.len();
    f.push(insn(0x15, nr as u32)); // JMP JEQ K
    let mut jumps = vec![first];
    for &(arg, mask, value) in args {
        f.push(insn(0x20, 16 + arg * 8));
        if mask != u32::MAX {
            f.push(insn(0x54, mask)); // ALU AND K
        }
        jumps.push(f.len());
        f.push(insn(0x15, value));
    }
    f.push(insn(0x06, NOTIFY));
    for i in jumps {
        f[i].jf = (f.len() - i - 1).try_into().unwrap();
    }
}

/// Install the one listener inherited by every Android process. The caller
/// must have set no_new_privs (or have the namespace's CAP_SYS_ADMIN).
/// Waits stay interruptible for ptrace and job control; binder restarts are
/// made idempotent by the broker.
#[allow(clippy::unnecessary_cast)] // RLIMIT_* is i32 on bionic, u32 on glibc.
pub fn install() -> std::io::Result<i32> {
    use libc::*;
    let mut f = vec![insn(0x20, 4), insn(0x15, 0xc000003e), insn(0x06, ALLOW)];
    f[1].jt = 1; // Other architectures retain the kernel's handling.
    rule(&mut f, SYS_openat, &[]);
    // OpenHarmony: musl opens with open(2), which the broker serves as
    // openat; its access-token device's ioctls are type 'A', ashmem's 0x77.
    if crate::guest() == crate::Guest::Ohos {
        rule(&mut f, SYS_open, &[]);
        for ty in [crate::ohos::IOCTL_TYPE, crate::ashmem::IOCTL_TYPE] {
            rule(&mut f, SYS_ioctl, &[(1, 0xff00, (ty as u32) << 8)]);
        }
    }
    rule(&mut f, SYS_inotify_add_watch, &[]);
    rule(&mut f, SYS_ioctl, &[(1, 0xff00, (b'E' as u32) << 8)]);
    rule(&mut f, SYS_ioctl, &[(1, 0xff00, (b'b' as u32) << 8)]);
    for nr in [
        SYS_getuid,
        SYS_geteuid,
        SYS_getgid,
        SYS_getegid,
        SYS_getresuid,
        SYS_getresgid,
        SYS_getgroups,
        SYS_setuid,
        SYS_setgid,
        SYS_setreuid,
        SYS_setregid,
        SYS_setresuid,
        SYS_setresgid,
        SYS_setfsuid,
        SYS_setfsgid,
        SYS_setgroups,
        SYS_chown,
        SYS_fchown,
        SYS_lchown,
        SYS_fchownat,
        SYS_readlinkat,
        SYS_readlink,
        SYS_getcwd,
        SYS_fstat,
        SYS_capget,
        SYS_capset,
        SYS_mkdir,
        SYS_mkdirat,
        SYS_chmod,
        SYS_fchmod,
        SYS_fchmodat,
        452, /* fchmodat2 */
    ] {
        rule(&mut f, nr, &[]);
    }
    for nr in [SYS_faccessat, SYS_faccessat2] {
        rule(&mut f, nr, &[(2, W_OK as u32, W_OK as u32)]);
    }
    rule(
        &mut f,
        SYS_newfstatat,
        &[
            (0, !0, AT_FDCWD as u32),
            (3, !0, AT_SYMLINK_NOFOLLOW as u32),
        ],
    );
    rule(
        &mut f,
        SYS_getsockopt,
        &[(1, !0, SOL_SOCKET as u32), (2, !0, SO_PEERCRED as u32)],
    );
    rule(
        &mut f,
        SYS_setsockopt,
        &[(1, !0, SOL_SOCKET as u32), (2, !0, SO_MARK as u32)],
    );
    for op in [PR_SET_SECUREBITS, PR_CAPBSET_DROP, PR_CAP_AMBIENT] {
        rule(&mut f, SYS_prctl, &[(0, !0, op as u32)]);
    }
    for nr in [SYS_bpf, SYS_statfs, SYS_getxattr, SYS_lgetxattr] {
        rule(&mut f, nr, &[]);
    }
    rule(
        &mut f,
        SYS_getsockopt,
        &[(1, !0, SOL_SOCKET as u32), (2, !0, SO_ANDROID_DROP_REASON)],
    );
    rule(&mut f, SYS_prlimit64, &[(1, !0, RLIMIT_MEMLOCK as u32)]);
    rule(&mut f, SYS_setrlimit, &[(0, !0, RLIMIT_MEMLOCK as u32)]);
    for clock in [CLOCK_REALTIME_ALARM, CLOCK_BOOTTIME_ALARM] {
        rule(&mut f, SYS_timerfd_create, &[(0, !0, clock as u32)]);
    }
    for policy in [SCHED_FIFO, SCHED_RR] {
        rule(
            &mut f,
            SYS_sched_setscheduler,
            &[(1, !(SCHED_RESET_ON_FORK as u32), policy as u32)],
        );
    }
    f.push(insn(0x06, ALLOW));
    let prog = Program {
        len: f.len().try_into().unwrap(),
        filter: f.as_ptr(),
    };
    let fd = unsafe { syscall(SYS_seccomp, 1, 8, &prog) } as i32;
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}
