//! innkeeper-brk, which pivot-exec execs twice (supervisor/README.md).
//! `--init <sync-fd> <program>...` installs the seccomp filter, sends its
//! listener over the sync socket and execs Android's init.
//! `<listener-fd> <pin-dir-fd> <init-pid>` serves that listener as init's
//! parent and exits with init's status.
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--init") {
        init(&args[1..]);
    }
    let fd = |i: usize| {
        args.get(i)
            .and_then(|a| a.to_str())
            .and_then(|s| s.parse::<i32>().ok())
            .expect("usage: innkeeper-brk <listener-fd> <pin-dir-fd> <init-pid>")
    };
    let listener = fd(0);
    let pins = fd(1);
    let init = fd(2);
    // pivot-exec execs us without changing pid. Reap orphans as its
    // subreaper, and end the session when init exits.
    std::thread::spawn(move || reap(init));
    unsafe { libc::umask(0) };
    innkeeper_broker::notify::serve(
        listener,
        (pins >= 0).then(|| unsafe { OwnedFd::from_raw_fd(pins) }),
    );
}
fn reap(init: i32) -> ! {
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == init {
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            };
            std::process::exit(code);
        }
        if pid < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            eprintln!("broker: waitpid: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
    }
}
fn init(args: &[std::ffi::OsString]) -> ! {
    let sock: i32 = args
        .first()
        .and_then(|a| a.to_str())
        .and_then(|a| a.parse().ok())
        .expect("missing init sync fd");
    let argv: Vec<_> = args[1..]
        .iter()
        .map(|s| std::ffi::CString::new(s.as_bytes()).unwrap())
        .collect();
    assert!(!argv.is_empty(), "missing Android program");
    let env: Vec<_> = std::env::vars_os()
        .map(|(k, v)| {
            let mut s = if k == "INNKEEPER_INIT_PRELOAD" {
                b"LD_PRELOAD".to_vec()
            } else {
                k.into_vec()
            };
            s.push(b'=');
            s.extend_from_slice(v.as_bytes());
            std::ffi::CString::new(s).unwrap()
        })
        .collect();
    let mut argv_ptr: Vec<_> = argv.iter().map(|s| s.as_ptr()).collect();
    argv_ptr.push(std::ptr::null());
    let mut env_ptr: Vec<_> = env.iter().map(|s| s.as_ptr()).collect();
    env_ptr.push(std::ptr::null());
    let listener = innkeeper_broker::filter::install().expect("install seccomp filter");
    unsafe {
        // A usize buffer gives cmsghdr its required alignment.
        let mut control = [0usize; 4];
        let mut byte = b'L';
        let mut io = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut io;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(size_of::<i32>() as _) as _;
        let cm = libc::CMSG_FIRSTHDR(&msg);
        (*cm).cmsg_level = libc::SOL_SOCKET;
        (*cm).cmsg_type = libc::SCM_RIGHTS;
        (*cm).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as _) as _;
        libc::CMSG_DATA(cm).cast::<i32>().write(listener);
        if libc::sendmsg(sock, &msg, 0) != 1 {
            libc::_exit(1);
        }
        libc::close(listener);
        libc::close(sock);
        libc::execve(argv[0].as_ptr(), argv_ptr.as_ptr(), env_ptr.as_ptr());
        libc::_exit(1);
    }
}
