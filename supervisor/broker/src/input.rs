//! Session-local evdev devices. EventHub polls a socket endpoint installed by
//! ADDFD; its peer receives the bridge's input_event records. Nothing reaches
//! the host input subsystem. The ordinary tmpfs nodes exist for inotify only.
pub mod gamepad;

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex};

use crate::notify::{
    SECCOMP_ADDFD_FLAG_SEND, SeccompNotif, SeccompNotifAddfd, mem_read, mem_write, notif_addfd_cmd,
    notif_id_valid, open_mem, read_path, send_resp,
};

pub const EVENT_SIZE: usize = 24;
pub const KEYBOARD: u32 = 0;
pub const MOUSE: u32 = 1;
// Flatpak's input grant mounts a host-owned directory at /dev/input. Discovery
// uses our own tmpfs directory; virtual names cannot shadow host eventN nodes.
const DIRECTORY: &str = "/dev/innkeeper-input";
const PREFIX: &[u8] = b"/dev/input/innkeeper-event";

/// x86_64 Linux input_event, with a CLOCK_MONOTONIC timeval. Kept explicit:
/// Rust struct padding is not part of the bridge protocol.
pub fn event(micros: i64, ty: u16, code: u16, value: i32) -> [u8; EVENT_SIZE] {
    let mut b = [0; EVENT_SIZE];
    b[..8].copy_from_slice(&(micros / 1_000_000).to_le_bytes());
    b[8..16].copy_from_slice(&(micros % 1_000_000).to_le_bytes());
    b[16..18].copy_from_slice(&ty.to_le_bytes());
    b[18..20].copy_from_slice(&code.to_le_bytes());
    b[20..].copy_from_slice(&value.to_le_bytes());
    b
}

struct Client {
    device: u32,
    peer: OwnedFd,
    /// Remember the installed socket's identity without retaining its endpoint:
    /// the peer must receive POLLHUP when Android closes the last reference.
    identity: std::path::PathBuf,
    pid: u32,
    fd: i32,
}
impl Client {
    fn live(&self) -> bool {
        let link = std::fs::read_link(format!("/proc/{}/fd/{}", self.pid, self.fd));
        link.as_ref().ok() == Some(&self.identity)
    }

    fn deliver(&self, packet: &[u8]) -> std::io::Result<()> {
        while self.live() {
            let n = unsafe {
                libc::send(
                    self.peer.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n >= 0 {
                // EventHub reads one packet per ready device, in epoll order.
                // Wait for this batch to be read before making another device
                // readable, preserving modifier/mouse order through InputReader.
                while self.live() {
                    let mut queued = 0i32;
                    if unsafe { libc::ioctl(self.peer.as_raw_fd(), libc::TIOCOUTQ, &mut queued) }
                        < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if queued == 0 {
                        break;
                    }
                    // Writable does not mean consumed on a Unix socket.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                return Ok(());
            } // SOCK_SEQPACKET preserves the whole record batch.
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    let mut poll = libc::pollfd {
                        fd: self.peer.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // Recheck the installed fd when a reader closes without
                    // draining, including closure of its installed fd.
                    let n = unsafe { libc::poll(&mut poll, 1, 100) };
                    if n < 0
                        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                _ => return Err(e),
            }
        }
        Ok(()) // The Android reader closed this device.
    }
}
struct Pad {
    identity: String,
    feedback: Option<gamepad::Feedback>,
    axes: [i32; 8],
}

struct Devices {
    wake: Option<UnixDatagram>,
    clients: Vec<Arc<Client>>,
    keys: HashMap<u32, [u8; 96]>,
    leds: [u8; 8],
    pads: HashMap<u32, Pad>,
}

pub(crate) struct Input {
    listener: RawFd,
    resp_sz: usize,
    devices: Arc<Mutex<Devices>>,
}

fn bitmap(size: usize, bits: impl IntoIterator<Item = usize>) -> Vec<u8> {
    let mut b = vec![0; size];
    for bit in bits {
        if bit / 8 < b.len() {
            b[bit / 8] |= 1 << (bit % 8);
        }
    }
    b
}

/// EVIOCG* answers, including the caller's requested bitmap/string length.
/// Return value is bytes copied for variable-length queries, zero for fixed
/// structs, as evdev does. Unknown ioctls stay ENOTTY.
fn answer(device: u32, cmd: u32, keys: &[u8], leds: &[u8]) -> Result<(Vec<u8>, i64), i32> {
    let nr = cmd & 0xff;
    let size = ((cmd >> 16) & 0x3fff) as usize;
    let read = cmd >> 30 & 2 != 0;
    let mut variable = false;
    let mut b = match (read, nr) {
        (true, 0x01) => 0x010001i32.to_le_bytes().to_vec(), // EV_VERSION
        (true, 0x02) => [6u16, 0x1209, 0x4c00 + u16::from(device != KEYBOARD), 1]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
        (true, 0x03) => [0i32, 0].into_iter().flat_map(i32::to_le_bytes).collect(),
        (true, 0x06..=0x08) => {
            variable = true;
            let s = match nr {
                0x06 => {
                    if device == KEYBOARD {
                        "Innkeeper Keyboard".to_owned()
                    } else {
                        format!("Innkeeper Mouse {}", device - 1)
                    }
                }
                0x07 => {
                    if device == KEYBOARD {
                        "innkeeper/keyboard".to_owned()
                    } else {
                        format!("innkeeper/mouse/{}", device - 1)
                    }
                }
                _ => {
                    if device == KEYBOARD {
                        "innkeeper-keyboard".to_owned()
                    } else {
                        format!("innkeeper-mouse-{}", device - 1)
                    }
                }
            };
            [s.as_bytes(), &[0]].concat()
        }
        (true, 0x09) => {
            variable = true;
            vec![0; size]
        } // EVIOCGPROP
        (true, 0x18) => {
            variable = true;
            keys.to_vec()
        }
        (true, 0x19) => {
            variable = true;
            leds.to_vec()
        }
        (true, 0x1b) => {
            variable = true;
            vec![0; size]
        } // EVIOCGSW
        (true, 0x20..=0x3f) => {
            variable = true;
            match nr - 0x20 {
                0 => bitmap(
                    size,
                    if device == KEYBOARD {
                        vec![0, 1, 17]
                    } else {
                        vec![0, 1, 2]
                    },
                ),
                1 => bitmap(
                    size,
                    if device == KEYBOARD {
                        (1..=248).chain([464]).collect::<Vec<_>>()
                    } else {
                        (272..=279).collect()
                    },
                ),
                2 if device != KEYBOARD => bitmap(size, [0, 1, 6, 8, 11, 12]),
                17 if device == KEYBOARD => bitmap(size, [0, 1, 2]),
                _ => vec![0; size],
            }
        }
        (false, 0x03 | 0x90 | 0xa0) => vec![], // repeat, grab, monotonic clock
        _ => return Err(libc::ENOTTY),
    };
    if variable {
        b.resize(size.min(b.len()), 0);
        let n = b.len() as i64;
        Ok((b, n))
    } else {
        Ok((b, 0))
    }
}

fn pad_answer(device: u32, pad: &Pad, cmd: u32, keys: &[u8]) -> Result<(Vec<u8>, i64), i32> {
    let nr = cmd & 0xff;
    let size = ((cmd >> 16) & 0x3fff) as usize;
    if cmd >> 30 & 2 == 0 {
        return answer(device, cmd, keys, &[]);
    }
    let variable = matches!(nr, 6..=9 | 0x18..=0x3f);
    let mut bytes = match nr {
        2 => [6u16, 0x1209, 0x4c02, 1]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
        6 => format!("Innkeeper Gamepad {}\0", device - gamepad::FIRST).into_bytes(),
        7 | 8 => format!("innkeeper/gamepad/{}\0", pad.identity).into_bytes(),
        0x20 => bitmap(
            size,
            if pad.feedback.is_some() {
                vec![0, 1, 3, 21]
            } else {
                vec![0, 1, 3]
            },
        ),
        0x21 => bitmap(size, gamepad::BUTTONS.map(usize::from)),
        0x23 => bitmap(size, gamepad::AXES.map(usize::from)),
        0x35 => bitmap(
            size,
            if pad.feedback.is_some() {
                vec![0x50]
            } else {
                vec![]
            },
        ),
        0x22 | 0x31 => vec![0; size],
        0x40..=0x7f => {
            let code = (nr - 0x40) as u16;
            let index = gamepad::AXES
                .iter()
                .position(|&a| a == code)
                .ok_or(libc::EINVAL)?;
            gamepad::absinfo(code, pad.axes[index]).unwrap()
        }
        _ => return answer(device, cmd, keys, &[]),
    };
    if variable {
        bytes.truncate(size);
        let n = bytes.len() as i64;
        Ok((bytes, n))
    } else {
        Ok((bytes, 0))
    }
}

// Registration carries an optional already-open rumble fd. Android receives
// only broker sockets; only the bridge reads the host controller's events.
fn receive(socket: &UnixDatagram, bytes: &mut [u8]) -> std::io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 4];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fd = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count =
                    ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<i32>();
                for i in 0..count {
                    let received = OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<i32>().add(i),
                    ));
                    if fd.is_none() {
                        fd = Some(received);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n as usize, fd))
}

impl Default for Devices {
    fn default() -> Self {
        Self {
            wake: None,
            clients: Vec::new(),
            keys: HashMap::from([(KEYBOARD, [0; 96])]),
            leds: [0; 8],
            pads: HashMap::new(),
        }
    }
}

impl Devices {
    fn changed(&self) {
        if let Some(wake) = &self.wake {
            // Nonblocking: an already queued notification also wakes the poll.
            let _ = wake.send(&[1]);
        }
    }

    fn remove(&mut self, device: u32, dir: &std::path::Path) {
        self.keys.remove(&device);
        self.pads.remove(&device);
        self.clients.retain(|c| c.device != device);
        self.changed();
        let _ = std::fs::remove_file(dir.join(format!("innkeeper-event{device}")));
    }
}

// Drain before an ioctl too: a following erase/update must not overtake
// EV_FF writes that the caller already queued on its installed socket.
fn drain_writes(client: &Client, state: &mut Devices) {
    let mut packet = [0; EVENT_SIZE * 64];
    loop {
        let n = unsafe {
            libc::recv(
                client.peer.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            break;
        }
        for ev in packet[..n as usize].as_chunks::<EVENT_SIZE>().0 {
            let ty = u16::from_le_bytes(ev[16..18].try_into().unwrap());
            let code = u16::from_le_bytes(ev[18..20].try_into().unwrap());
            let value = i32::from_le_bytes(ev[20..24].try_into().unwrap());
            if ty == 17 && code < 64 {
                let byte = &mut state.leds[usize::from(code / 8)];
                if value == 0 {
                    *byte &= !(1 << (code % 8));
                } else {
                    *byte |= 1 << (code % 8);
                }
            } else if ty == 21
                && let Some(feedback) = state
                    .pads
                    .get(&client.device)
                    .and_then(|p| p.feedback.as_ref())
                && let Err(e) = feedback.write(ev)
            {
                eprintln!("evdev: rumble write: {e}");
            }
        }
    }
}

fn reap_closed_clients(state: &mut Devices) {
    for client in std::mem::take(&mut state.clients) {
        if client.live() {
            state.clients.push(client);
            continue;
        }
        // Preserve queued play/stop ordering before emulating evdev's per-open
        // effect cleanup. The controller's host fd outlives virtual clients.
        drain_writes(&client, state);
        if let Some(feedback) = state
            .pads
            .get_mut(&client.device)
            .and_then(|p| p.feedback.as_mut())
        {
            feedback.close_client(client.peer.as_raw_fd());
        }
    }
}

// New opens/removals wake the snapshot poll; writes and final closes wake
// directly on the peer. There is no timer when Android has no input work.
fn wait_for_writes(devices: &Mutex<Devices>, wake: &UnixDatagram) -> std::io::Result<()> {
    let clients = devices.lock().unwrap().clients.clone();
    let mut polls: Vec<_> = std::iter::once(wake.as_raw_fd())
        .chain(clients.iter().map(|c| c.peer.as_raw_fd()))
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, -1) } < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    while wake.recv(&mut [0]).is_ok() {}
    let mut state = devices.lock().unwrap();
    reap_closed_clients(&mut state);
    for (client, poll) in clients.iter().zip(&polls[1..]) {
        if poll.revents & libc::POLLIN != 0 && state.clients.iter().any(|c| Arc::ptr_eq(c, client))
        {
            drain_writes(client, &mut state);
        }
    }
    Ok(())
}

fn register_pad(state: &mut Devices, device: u32, pad: Pad, dir: &std::path::Path) {
    let replaced = state.pads.insert(device, pad).is_some();
    let new = state.keys.insert(device, [0; 96]).is_none();
    let path = dir.join(format!("innkeeper-event{device}"));
    if replaced {
        // EventHub caches an effect ID for its open. Reopen after replacing the
        // host fd, whose RAII owner has erased and forgotten the old effects.
        state.clients.retain(|c| c.device != device);
        state.changed();
        let _ = std::fs::remove_file(&path);
    }
    if new || replaced {
        let _ = std::fs::write(path, []);
    }
}

fn prepare_directories(dev: &std::path::Path) -> std::io::Result<()> {
    // EventHub checks existence before its redirected scan. Flatpak only
    // supplies /dev/input when the host has one; leave an existing bind intact.
    std::fs::create_dir_all(dev.join("input"))?;
    std::fs::create_dir_all(dev.join("innkeeper-input"))
}

// Registration has a fixed eight-digit port identity. Event packets are
// 4 + N*EVENT_SIZE bytes, so timestamp bytes can never look like control data.
fn is_registration(packet: &[u8]) -> bool {
    packet.len() == 14 && matches!(packet[4], 2 | 3)
}

impl Input {
    pub(crate) fn new(listener: RawFd, resp_sz: usize) -> Option<Self> {
        let dir = std::path::Path::new(DIRECTORY);
        prepare_directories(std::path::Path::new("/dev")).ok()?;
        let path = "/dev/socket/innkeeper/evdev.sock";
        let _ = std::fs::remove_file(path);
        let socket = UnixDatagram::bind(path).ok()?;
        let (wake, changed) = UnixDatagram::pair().ok()?;
        wake.set_nonblocking(true).ok()?;
        changed.set_nonblocking(true).ok()?;
        let devices = Arc::new(Mutex::new(Devices {
            wake: Some(changed),
            ..Devices::default()
        }));
        let writes = Arc::clone(&devices);
        std::thread::spawn(move || while wait_for_writes(&writes, &wake).is_ok() {});
        let state = Arc::clone(&devices);
        std::thread::spawn(move || {
            let mut buf = [0; 4 + 64 * EVENT_SIZE];
            loop {
                let Ok((n, feedback)) = receive(&socket, &mut buf) else {
                    return;
                };
                if n < 4 {
                    continue;
                }
                let device = u32::from_le_bytes(buf[..4].try_into().unwrap());
                let mut s = state.lock().unwrap();
                if device >= gamepad::FIRST && is_registration(&buf[..n]) {
                    let Ok(identity) = std::str::from_utf8(&buf[6..n]) else {
                        continue;
                    };
                    let feedback = feedback.map(|fd| gamepad::Feedback::new(fd, buf[4] == 3));
                    register_pad(
                        &mut s,
                        device,
                        Pad {
                            identity: identity.into(),
                            feedback,
                            axes: [0; 8],
                        },
                        dir,
                    );
                    continue;
                }
                if n == 5 && device != KEYBOARD {
                    let path = dir.join(format!("innkeeper-event{device}"));
                    if buf[4] != 0 {
                        if let std::collections::hash_map::Entry::Vacant(e) = s.keys.entry(device) {
                            e.insert([0; 96]);
                            let _ = std::fs::write(path, []);
                        }
                    } else {
                        s.remove(device, dir);
                    }
                    continue;
                }
                if (n - 4) % EVENT_SIZE != 0 || !s.keys.contains_key(&device) {
                    continue;
                }
                for ev in buf[4..n].as_chunks::<EVENT_SIZE>().0 {
                    let ty = u16::from_le_bytes(ev[16..18].try_into().unwrap());
                    let code = u16::from_le_bytes(ev[18..20].try_into().unwrap()) as usize;
                    let value = i32::from_le_bytes(ev[20..24].try_into().unwrap());
                    if ty == 3
                        && let Some(pad) = s.pads.get_mut(&device)
                        && let Some(i) = gamepad::AXES.iter().position(|&a| usize::from(a) == code)
                    {
                        pad.axes[i] = value;
                    }
                    if ty == 1 && code < 768 {
                        let b = &mut s.keys.get_mut(&device).unwrap()[code / 8];
                        if value == 0 {
                            *b &= !(1 << (code % 8));
                        } else {
                            *b |= 1 << (code % 8);
                        }
                    }
                }
                let clients: Vec<_> = s
                    .clients
                    .iter()
                    .filter(|c| c.device == device)
                    .cloned()
                    .collect();
                drop(s);
                // Do not hold device state while waiting: EventHub may need an
                // ioctl answer before it can drain its socket.
                for client in clients {
                    if let Err(e) = client.deliver(&buf[4..n]) {
                        eprintln!("evdev: delivery failed: {e}");
                    }
                }
            }
        });
        // Create after the input socket is ready; EventHub observes these via
        // its initial scan or inotify. /dev is the sandbox's private tmpfs.
        for n in 0..1 {
            std::fs::write(dir.join(format!("innkeeper-event{n}")), []).ok()?;
        }
        Some(Self {
            listener,
            resp_sz,
            devices,
        })
    }

    pub(crate) fn serve(&self, req: &SeccompNotif) -> bool {
        let nr = req.data.nr as i64;
        if nr == libc::SYS_openat || nr == libc::SYS_inotify_add_watch {
            let path = read_path(req.pid as i32, req.data.args[1]);
            if path.as_deref() == Some(b"/dev/input") {
                if nr == libc::SYS_inotify_add_watch {
                    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, req.pid, 0) } as i32;
                    let result = if pidfd < 0 {
                        -1
                    } else {
                        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
                        let fd = unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_getfd,
                                pidfd.as_raw_fd(),
                                req.data.args[0] as i32,
                                0,
                            )
                        } as i32;
                        if fd < 0 {
                            -1
                        } else {
                            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                            unsafe {
                                libc::inotify_add_watch(
                                    fd.as_raw_fd(),
                                    c"/dev/innkeeper-input".as_ptr(),
                                    req.data.args[2] as u32,
                                )
                            }
                        }
                    };
                    let error = if result < 0 {
                        -std::io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(libc::EIO)
                    } else {
                        0
                    };
                    send_resp(
                        self.listener,
                        self.resp_sz,
                        req.id,
                        i64::from(result.max(0)),
                        error,
                        false,
                    );
                    return true;
                }
                let Ok(dir) = std::fs::File::open(DIRECTORY) else {
                    return false;
                };
                let add = SeccompNotifAddfd {
                    id: req.id,
                    flags: SECCOMP_ADDFD_FLAG_SEND,
                    srcfd: dir.as_raw_fd() as u32,
                    newfd: 0,
                    newfd_flags: req.data.args[2] as u32 & libc::O_CLOEXEC as u32,
                };
                unsafe {
                    libc::ioctl(
                        self.listener,
                        notif_addfd_cmd(size_of::<SeccompNotifAddfd>()) as _,
                        &add,
                    );
                }
                return true;
            }
        }
        let device = if req.data.nr as i64 == libc::SYS_openat {
            let path = read_path(req.pid as i32, req.data.args[1]);
            let device = path
                .as_deref()
                .and_then(|p| p.strip_prefix(PREFIX))
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|p| p.parse::<u32>().ok());
            let Some(device) = device else { return false };
            if !self.devices.lock().unwrap().keys.contains_key(&device) {
                return false;
            }
            device
        } else if req.data.nr as i64 == libc::SYS_ioctl
            && req.data.args[1] as u32 & 0xff00 == 0x4500
        {
            let path = format!("/proc/{}/fd/{}", req.pid, req.data.args[0]);
            let Ok(link) = std::fs::read_link(path) else {
                return false;
            };
            let mut state = self.devices.lock().unwrap();
            let Some(c) = state.clients.iter().find(|c| c.identity == link).cloned() else {
                return false;
            };
            let Some(mem) = open_mem(req.pid as i32) else {
                send_resp(self.listener, self.resp_sz, req.id, 0, -libc::EFAULT, false);
                return true;
            };
            if !notif_id_valid(self.listener, req.id) {
                return true;
            }
            drain_writes(&c, &mut state);
            let cmd = req.data.args[1] as u32;
            let Devices {
                pads, keys, leds, ..
            } = &mut *state;
            let result = if let Some(pad) = pads.get_mut(&c.device) {
                if let Some(feedback) = &mut pad.feedback {
                    match cmd & 0xff {
                        0x80 | 0x81 | 0x84 => {
                            let len = match cmd & 0xff {
                                0x80 => gamepad::EFFECT_SIZE,
                                0x84 => 4,
                                _ => 0,
                            };
                            let data = if cmd & 0xff == 0x80 {
                                mem_read(mem.raw(), req.data.args[2], len).ok_or(libc::EFAULT)
                            } else {
                                Ok(vec![0; len])
                            };
                            data.and_then(|mut data| {
                                if cmd & 0xff == 0x80
                                    && u16::from_le_bytes(data[..2].try_into().unwrap()) != 0x50
                                {
                                    return Err(libc::EINVAL);
                                }
                                let val = feedback.ioctl(
                                    c.peer.as_raw_fd(),
                                    cmd,
                                    &mut data,
                                    req.data.args[2],
                                )?;
                                Ok((data, val))
                            })
                        }
                        _ => pad_answer(c.device, pad, cmd, &keys[&c.device]),
                    }
                } else {
                    pad_answer(c.device, pad, cmd, &keys[&c.device])
                }
            } else {
                answer(c.device, cmd, &keys[&c.device], leds)
            };
            match result {
                Ok((bytes, val)) if mem_write(mem.raw(), req.data.args[2], &bytes) => {
                    send_resp(self.listener, self.resp_sz, req.id, val, 0, false)
                }
                Ok(_) => send_resp(self.listener, self.resp_sz, req.id, 0, -libc::EFAULT, false),
                Err(e) => send_resp(self.listener, self.resp_sz, req.id, 0, -e, false),
            };
            return true;
        } else {
            return false;
        };
        let mut fds = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                fds.as_mut_ptr(),
            )
        } != 0
        {
            send_resp(self.listener, self.resp_sz, req.id, 0, -libc::ENOMEM, false);
            return true;
        }
        let endpoint = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let peer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let add = SeccompNotifAddfd {
            id: req.id,
            flags: SECCOMP_ADDFD_FLAG_SEND,
            srcfd: endpoint.as_raw_fd() as u32,
            newfd: 0,
            newfd_flags: req.data.args[2] as u32 & libc::O_CLOEXEC as u32,
        };
        // Serialize installation with dynamic removal, so a newly installed
        // endpoint always has live ioctl state until removal closes it.
        let mut state = self.devices.lock().unwrap();
        if !state.keys.contains_key(&device) {
            send_resp(self.listener, self.resp_sz, req.id, 0, -libc::ENOENT, false);
            return true;
        }
        let fd = unsafe {
            libc::ioctl(
                self.listener,
                notif_addfd_cmd(size_of::<SeccompNotifAddfd>()) as _,
                &add,
            )
        };
        if fd >= 0 {
            state.clients.push(Arc::new(Client {
                device,
                peer,
                identity: std::fs::read_link(format!("/proc/self/fd/{}", endpoint.as_raw_fd()))
                    .expect("installed input socket"),
                pid: req.pid,
                fd,
            }));
            state.changed();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(device: u32) -> (Client, OwnedFd) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let client = Client {
            device,
            peer: unsafe { OwnedFd::from_raw_fd(fds[1]) },
            identity: std::fs::read_link(format!("/proc/self/fd/{}", reader.as_raw_fd())).unwrap(),
            pid: std::process::id(),
            fd: reader.as_raw_fd(),
        };
        (client, reader)
    }

    #[test]
    fn idle_input_waits_for_new_clients_writes_and_final_close() {
        let (wake, changed) = UnixDatagram::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        changed.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(Devices {
            wake: Some(changed),
            ..Devices::default()
        }));
        let shared = Arc::clone(&state);
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            for _ in 0..3 {
                wait_for_writes(&shared, &wake).unwrap();
                done.send(()).unwrap();
            }
        });
        let idle = std::time::Duration::from_millis(50);
        let deadline = std::time::Duration::from_secs(1);
        assert!(completed.recv_timeout(idle).is_err());
        let (client, reader) = client(KEYBOARD);
        {
            let mut state = state.lock().unwrap();
            state.clients.push(Arc::new(client));
            state.changed();
        }
        completed.recv_timeout(deadline).unwrap();
        assert!(completed.recv_timeout(idle).is_err());
        let packet = event(0, 17, 0, 1); // keyboard LED write
        assert_eq!(
            unsafe { libc::send(reader.as_raw_fd(), packet.as_ptr().cast(), packet.len(), 0) },
            packet.len() as isize
        );
        completed.recv_timeout(deadline).unwrap();
        assert_eq!(state.lock().unwrap().leds[0], 1);
        drop(reader);
        completed.recv_timeout(deadline).unwrap();
        assert!(state.lock().unwrap().clients.is_empty());
        worker.join().unwrap();
    }

    fn receive(fd: &OwnedFd) -> Vec<u8> {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
        let mut packet = vec![0; EVENT_SIZE];
        assert_eq!(
            unsafe { libc::recv(fd.as_raw_fd(), packet.as_mut_ptr().cast(), packet.len(), 0) },
            EVENT_SIZE as isize
        );
        packet
    }

    #[test]
    fn modifier_release_is_consumed_before_mouse_report() {
        let (keyboard, keys) = client(KEYBOARD);
        let (mouse, motion) = client(MOUSE);
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            keyboard.deliver(&event(1, 1, 42, 1)).unwrap();
            keyboard.deliver(&event(2, 1, 42, 0)).unwrap();
            mouse.deliver(&event(3, 2, 0, 10)).unwrap();
            done.send(()).unwrap();
        });
        assert!(
            completed
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err()
        );
        assert_eq!(receive(&keys), event(1, 1, 42, 1));
        // The queued release must be read before mouse delivery can begin.
        let mut poll = libc::pollfd {
            fd: motion.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 20) }, 0);
        assert_eq!(receive(&keys), event(2, 1, 42, 0));
        assert_eq!(receive(&motion), event(3, 2, 0, 10));
        completed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
    }
    fn get(nr: u32, size: u32) -> u32 {
        0x80004500 | nr | size << 16
    }
    #[test]
    fn discovery_exists_without_host_input_and_preserves_host_nodes() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../out/test/input-directories-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!root.join("input").exists());
        prepare_directories(&root).unwrap();
        assert!(root.join("input").is_dir()); // EventHub's existence check
        assert!(root.join("innkeeper-input").is_dir()); // redirected discovery
        std::fs::write(root.join("input/event0"), b"host node").unwrap();
        prepare_directories(&root).unwrap();
        assert_eq!(
            std::fs::read(root.join("input/event0")).unwrap(),
            b"host node"
        );
        assert_eq!(
            std::fs::read_dir(root.join("innkeeper-input"))
                .unwrap()
                .count(),
            0
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn feedback_source() -> (gamepad::Feedback, std::thread::JoinHandle<()>) {
        let (feedback, source) = UnixDatagram::pair().unwrap();
        for socket in [&feedback, &source] {
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
        }
        let worker = std::thread::spawn(move || {
            let mut request = [0; 128];
            let n = source.recv(&mut request).unwrap();
            assert_eq!(n, 12 + gamepad::EFFECT_SIZE);
            assert_eq!(&request[..4], &0x40304580u32.to_le_bytes());
            assert_eq!(&request[14..16], &(-1i16).to_le_bytes());
            let mut response = vec![0; 4];
            response.extend_from_slice(&request[12..n]);
            response[6..8].copy_from_slice(&7i16.to_le_bytes());
            source.send(&response).unwrap();
            assert_eq!(source.recv(&mut request).unwrap(), 12);
            assert_eq!(&request[..4], &0x40044581u32.to_le_bytes());
            assert_eq!(&request[4..12], &7u64.to_le_bytes());
            source.send(&0i32.to_le_bytes()).unwrap();
        });
        (gamepad::Feedback::new(feedback.into(), true), worker)
    }

    fn upload(feedback: &mut gamepad::Feedback, client: &Client) {
        let mut effect = [0; gamepad::EFFECT_SIZE];
        effect[..2].copy_from_slice(&0x50u16.to_le_bytes());
        effect[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        assert_eq!(
            feedback.ioctl(client.peer.as_raw_fd(), 0x40304580, &mut effect, 0),
            Ok(0)
        );
    }

    #[test]
    fn closing_a_virtual_client_erases_its_uploaded_effects() {
        let (mut feedback, worker) = feedback_source();
        let (client, reader) = client(gamepad::FIRST);
        upload(&mut feedback, &client);
        let mut state = Devices::default();
        state.pads.insert(
            gamepad::FIRST,
            Pad {
                identity: "controller".into(),
                feedback: Some(feedback),
                axes: [0; 8],
            },
        );
        state.clients.push(Arc::new(client));
        reap_closed_clients(&mut state);
        assert_eq!(state.clients.len(), 1); // still open: keep the effect
        drop(reader);
        reap_closed_clients(&mut state);
        assert!(state.clients.is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn replacing_feedback_erases_old_effects_and_rediscovers_with_no_stale_effect() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../out/test/input-replacement-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pad = |feedback| Pad {
            identity: "controller".into(),
            feedback: Some(feedback),
            axes: [0; 8],
        };
        let (mut old_feedback, old_source) = feedback_source();
        let (old_client, old_reader) = client(gamepad::FIRST);
        upload(&mut old_feedback, &old_client);
        let mut state = Devices::default();
        register_pad(&mut state, gamepad::FIRST, pad(old_feedback), &dir);
        state.clients.push(Arc::new(old_client));

        let watch = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(watch >= 0);
        let watch = unsafe { OwnedFd::from_raw_fd(watch) };
        let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    watch.as_raw_fd(),
                    path.as_ptr(),
                    libc::IN_DELETE | libc::IN_CREATE,
                )
            } >= 0
        );
        let (new_feedback, new_source) = feedback_source();
        register_pad(&mut state, gamepad::FIRST, pad(new_feedback), &dir);
        old_source.join().unwrap(); // RAII erased through the old fd, before it closed
        assert!(state.clients.is_empty()); // EventHub's cached effect ID must be discarded
        let mut events = [0u8; 256];
        let n = unsafe { libc::read(watch.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
        assert!(n > 0);
        let mut masks = Vec::new();
        let mut offset = 0;
        while offset < n as usize {
            let ev = unsafe {
                std::ptr::read_unaligned(events.as_ptr().add(offset).cast::<libc::inotify_event>())
            };
            masks.push(ev.mask);
            offset += size_of::<libc::inotify_event>() + ev.len as usize;
        }
        assert_eq!(masks, [libc::IN_DELETE, libc::IN_CREATE]);

        let (new_client, new_reader) = client(gamepad::FIRST);
        upload(
            state
                .pads
                .get_mut(&gamepad::FIRST)
                .unwrap()
                .feedback
                .as_mut()
                .unwrap(),
            &new_client,
        );
        state.clients.push(Arc::new(new_client));
        drop(old_reader);
        reap_closed_clients(&mut state);
        assert_eq!(state.clients.len(), 1); // old close cannot erase the new fd's effect 7
        drop(new_reader);
        reap_closed_clients(&mut state);
        new_source.join().unwrap(); // only this client's close erases the new effect 7
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eventhub_identifies_keyboard_and_mouse() {
        assert_eq!(
            answer(KEYBOARD, get(2, 8), &[], &[]).unwrap().0,
            [6, 0, 9, 18, 0, 76, 1, 0]
        );
        assert_eq!(
            answer(KEYBOARD, get(6, 8), &[], &[]).unwrap(),
            (b"Innkeepe".to_vec(), 8)
        );
        let keys = answer(KEYBOARD, get(0x21, 96), &[], &[]).unwrap().0;
        assert_ne!(keys[30 / 8] & (1 << (30 % 8)), 0); // KEY_A: alphabetic keyboard
        assert_eq!(keys[272 / 8] & 1, 0);
        let rel = answer(MOUSE, get(0x22, 8), &[], &[]).unwrap().0;
        assert_eq!(rel[0] & 3, 3); // REL_X/Y: cursor device
        assert_ne!(rel[11 / 8] & (1 << (11 % 8)), 0); // high-resolution wheel
        assert!(answer(MOUSE, get(0x40, 24), &[], &[]).is_err());
    }
    #[test]
    fn display_mice_have_distinct_permanent_ports() {
        assert_eq!(
            answer(3, get(7, 80), &[], &[]).unwrap().0,
            b"innkeeper/mouse/2\0"
        );
        assert_eq!(
            answer(8, get(7, 80), &[], &[]).unwrap().0,
            b"innkeeper/mouse/7\0"
        );
        assert_ne!(
            answer(3, get(8, 80), &[], &[]).unwrap().0,
            answer(8, get(8, 80), &[], &[]).unwrap().0
        );
        assert_eq!(
            answer(3, get(0x22, 8), &[], &[]),
            answer(8, get(0x22, 8), &[], &[])
        );
    }
    #[test]
    fn removing_one_display_mouse_keeps_the_other_live() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../out/test/broker-mouse-removal");
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = Devices::default();
        let (first, first_fd) = client(3);
        let (second, _second_fd) = client(8);
        state.clients.extend([Arc::new(first), Arc::new(second)]);
        for device in [3, 8] {
            state.keys.insert(device, [0; 96]);
            std::fs::write(dir.join(format!("innkeeper-event{device}")), []).unwrap();
        }
        state.remove(8, &dir);
        assert!(!dir.join("innkeeper-event8").exists());
        assert!(!state.keys.contains_key(&8));
        assert_eq!(state.clients.len(), 1);
        assert!(dir.join("innkeeper-event3").exists());
        assert!(state.keys.contains_key(&3));
        let packet = event(0, 2, 0, 1);
        let remaining = state.clients[0].clone();
        let delivery = std::thread::spawn(move || remaining.deliver(&packet).unwrap());
        assert_eq!(receive(&first_fd), packet);
        delivery.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn gamepad_timestamp_bytes_never_register_a_device() {
        let mut registration = gamepad::FIRST.to_le_bytes().to_vec();
        registration.extend_from_slice(b"\x02\x001234abcd");
        assert!(is_registration(&registration));
        for seconds in [258, 259] {
            let mut packet = gamepad::FIRST.to_le_bytes().to_vec();
            packet.extend_from_slice(&event(seconds * 1_000_000, 1, 304, 1));
            packet.extend_from_slice(&event(seconds * 1_000_000, 0, 0, 0));
            assert!(matches!(packet[4], 2 | 3));
            assert!(!is_registration(&packet));
        }
    }

    #[test]
    fn gamepads_have_separate_ports_and_android_capabilities() {
        let pad = Pad {
            identity: "controller-one".into(),
            feedback: None,
            axes: [123, 0, 0, 0, 0, 0, 0, 0],
        };
        let first = pad_answer(gamepad::FIRST, &pad, get(8, 128), &[])
            .unwrap()
            .0;
        let other = Pad {
            identity: "controller-two".into(),
            feedback: None,
            axes: [0; 8],
        };
        assert_ne!(
            first,
            pad_answer(gamepad::FIRST + 1, &other, get(8, 128), &[])
                .unwrap()
                .0
        );
        let keys = pad_answer(gamepad::FIRST, &pad, get(0x21, 96), &[])
            .unwrap()
            .0;
        assert_ne!(keys[304 / 8] & 1, 0); // BUTTON_A
        assert_eq!(keys[30 / 8] & (1 << (30 % 8)), 0); // not an alphabetic keyboard
        let abs = pad_answer(gamepad::FIRST, &pad, get(0x40, 24), &[])
            .unwrap()
            .0;
        let values: Vec<_> = abs
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes(*b))
            .collect();
        assert_eq!(values, [123, -32768, 32767, 0, 4096, 0]);
        assert!(pad_answer(gamepad::FIRST, &pad, get(0x43, 24), &[]).is_err());
        assert_eq!(
            pad_answer(gamepad::FIRST, &pad, get(0x35, 16), &[])
                .unwrap()
                .0,
            vec![0; 16]
        );
    }

    #[test]
    fn records_have_the_kernel_abi_and_sync_boundary() {
        let b = event(12_345_678, 1, 30, 1);
        assert_eq!(&b[..8], &12i64.to_le_bytes());
        assert_eq!(&b[8..16], &345_678i64.to_le_bytes());
        assert_eq!(&b[16..], &[1, 0, 30, 0, 1, 0, 0, 0]);
        assert_eq!(&event(12_345_678, 0, 0, 0)[16..], &[0; 8]);
    }
}
