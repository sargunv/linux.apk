//! Host evdev controllers, outside the Android syscall filter. Linux's gamepad
//! codes are already a standard layout; normalize the common stick/trigger
//! axis arrangements to Android Generic.kl. No injected gamepad events exist.
use super::evdev::Evdev;
use innkeeper_broker::input::{
    EVENT_SIZE,
    gamepad::{BUTTONS, FIRST},
};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn get(fd: &impl AsRawFd, nr: u32, bytes: &mut [u8]) -> io::Result<()> {
    let cmd = 0x80004500u32 | nr | (bytes.len() as u32) << 16;
    if unsafe { libc::ioctl(fd.as_raw_fd(), cmd as _, bytes.as_mut_ptr()) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn bit(bytes: &[u8], n: usize) -> bool {
    bytes.get(n / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}
fn string(fd: &impl AsRawFd, nr: u32) -> String {
    let mut bytes = [0; 256];
    if get(fd, nr, &mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or_default()).into_owned()
}

#[derive(Clone, Copy, Debug)]
struct Axis {
    host: u16,
    target: u16,
    min: i32,
    max: i32,
}
impl Axis {
    fn value(self, value: i32) -> i32 {
        if self.target >= 16 {
            return value.clamp(-1, 1);
        }
        let unit = (i64::from(value) - i64::from(self.min))
            .clamp(0, i64::from(self.max) - i64::from(self.min));
        let range = (i64::from(self.max) - i64::from(self.min)).max(1);
        if self.target == 9 || self.target == 10 {
            (unit * 32767 / range) as i32
        } else {
            (unit * 65535 / range - 32768) as i32
        }
    }
}
struct Mapping {
    axes: Vec<Axis>,
    dpad: [bool; 4],
    playstation: bool,
}
impl Mapping {
    fn new(axes: &[(u16, i32, i32)], vendor: u16) -> Self {
        // EVIOCGABS succeeds with zeroed info even for unsupported axes.
        let right_xy = [3, 4].iter().all(|&code| {
            axes.iter()
                .any(|&(host, min, max)| host == code && min < max)
        });
        let axes = axes
            .iter()
            .filter_map(|&(host, min, max)| {
                let target = match host {
                    0 | 1 | 9 | 10 | 16 | 17 => host,
                    3 if right_xy => 2,
                    4 if right_xy => 5,
                    2 if right_xy => 10,
                    5 if right_xy => 9,
                    2 | 5 => host,
                    _ => return None,
                };
                (min < max).then_some(Axis {
                    host,
                    target,
                    min,
                    max,
                })
            })
            .collect();
        Self {
            axes,
            dpad: [false; 4],
            playstation: vendor == 0x054c,
        }
    }
    fn translate(&mut self, ty: u16, code: u16, value: i32) -> Option<(u16, u16, i32)> {
        // hid-playstation uses position names for north/west. Generic.kl uses
        // Xbox's BTN_X/BTN_Y aliases for those same numeric codes instead.
        let code = if ty == 1 && self.playstation {
            match code {
                307 => 308,
                308 => 307,
                _ => code,
            }
        } else {
            code
        };
        match ty {
            1 if BUTTONS.contains(&code) => Some((1, code, value)),
            1 if matches!(code, 312 | 313)
                && self
                    .axes
                    .iter()
                    .any(|a| a.target == if code == 312 { 10 } else { 9 }) =>
            {
                None
            }
            1 if matches!(code, 312 | 313) => Some((
                3,
                if code == 312 { 10 } else { 9 },
                if value == 0 { 0 } else { 32767 },
            )),
            1 if (544..=547).contains(&code) => {
                self.dpad[usize::from(code - 544)] = value != 0;
                let (negative, positive, axis) = if code < 546 { (0, 1, 17) } else { (2, 3, 16) };
                Some((
                    3,
                    axis,
                    i32::from(self.dpad[positive]) - i32::from(self.dpad[negative]),
                ))
            }
            3 => self
                .axes
                .iter()
                .find(|a| a.host == code)
                .map(|a| (3, a.target, a.value(value))),
            _ => None,
        }
    }
}

struct Controller {
    fd: OwnedFd,
    // A separate open owns FF effects so unplug/bridge shutdown releases them.
    feedback: Option<OwnedFd>,
    identity: String,
    id: u32,
    mapping: Mapping,
    pending: Vec<u8>,
    report: Vec<(u16, u16, i32)>,
    dropped: bool,
    synthetic: bool,
}
fn connect(path: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    for (a, &b) in addr.sun_path.iter_mut().zip(bytes) {
        *a = b as _;
    }
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            size_of::<libc::sockaddr_un>() as _,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let timeout = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            size_of::<libc::timeval>() as _,
        );
    }
    Ok(fd)
}
impl Controller {
    fn open(path: &Path, synthetic: bool) -> io::Result<Self> {
        let (fd, feedback, identity, mapping) = if synthetic {
            let fd = connect(path)?;
            let feedback = connect(&path.with_extension("ff"))?;
            let mapping = Mapping::new(
                &[
                    (0, -32768, 32767),
                    (1, -32768, 32767),
                    (2, 0, 255),
                    (3, -32768, 32767),
                    (4, -32768, 32767),
                    (5, 0, 255),
                    (16, -1, 1),
                    (17, -1, 1),
                ],
                0x045e,
            );
            (
                fd,
                Some(feedback),
                path.to_string_lossy().into_owned(),
                mapping,
            )
        } else {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)?;
            let clock = libc::CLOCK_MONOTONIC;
            unsafe {
                libc::ioctl(file.as_raw_fd(), 0x400445a0u32 as libc::c_ulong, &clock);
            }
            let mut keys = [0; 96];
            get(&file, 0x21, &mut keys)?;
            if !bit(&keys, 304) {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            let mut axes = Vec::new();
            for code in [0, 1, 2, 3, 4, 5, 9, 10, 16, 17] {
                let mut info = [0; 24];
                if get(&file, 0x40 + u32::from(code), &mut info).is_ok() {
                    axes.push((
                        code,
                        i32::from_le_bytes(info[4..8].try_into().unwrap()),
                        i32::from_le_bytes(info[8..12].try_into().unwrap()),
                    ));
                }
            }
            if !axes.iter().any(|a| a.0 == 0) || !axes.iter().any(|a| a.0 == 1) {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            let uniq = string(&file, 8);
            let phys = string(&file, 7);
            let mut id = [0; 8];
            get(&file, 2, &mut id)?;
            let identity = format!(
                "{:02x?}:{}",
                id,
                if !uniq.is_empty() {
                    uniq
                } else if !phys.is_empty() {
                    phys
                } else {
                    path.to_string_lossy().into_owned()
                }
            );
            let feedback = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
                .ok()
                .and_then(|f| {
                    let mut bits = [0; 16];
                    (get(&f, 0x35, &mut bits).is_ok() && bit(&bits, 0x50)).then(|| f.into())
                });
            (
                file.into(),
                feedback,
                identity,
                Mapping::new(&axes, u16::from_le_bytes(id[2..4].try_into().unwrap())),
            )
        };
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Ok(Self {
            fd,
            feedback,
            identity,
            id: 0,
            mapping,
            pending: Vec::new(),
            report: Vec::new(),
            dropped: false,
            synthetic,
        })
    }
    fn snapshot(&mut self) {
        if self.synthetic {
            return;
        }
        self.report.clear();
        let mut keys = [0; 96];
        if get(&self.fd, 0x18, &mut keys).is_ok() {
            for code in BUTTONS.into_iter().chain([312, 313, 544, 545, 546, 547]) {
                if let Some(ev) =
                    self.mapping
                        .translate(1, code, i32::from(bit(&keys, usize::from(code))))
                {
                    self.report.push(ev);
                }
            }
        }
        for axis in &self.mapping.axes {
            let mut info = [0; 24];
            if get(&self.fd, 0x40 + u32::from(axis.host), &mut info).is_ok() {
                self.report.push((
                    3,
                    axis.target,
                    axis.value(i32::from_le_bytes(info[..4].try_into().unwrap())),
                ));
            }
        }
    }
    fn flush(&mut self, writer: &Evdev, micros: i64) -> io::Result<()> {
        for events in self.report.chunks(63) {
            writer.send(self.id, micros, events)?;
        }
        self.report.clear();
        Ok(())
    }
    fn read(&mut self, writer: &Evdev) -> io::Result<()> {
        let mut bytes = [0; EVENT_SIZE * 64];
        loop {
            let n =
                unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            self.pending.extend_from_slice(&bytes[..n as usize]);
            let count = self.pending.len() / EVENT_SIZE * EVENT_SIZE;
            let records: Vec<_> = self.pending.drain(..count).collect();
            for ev in records.as_chunks::<EVENT_SIZE>().0 {
                let micros = i64::from_le_bytes(ev[..8].try_into().unwrap()) * 1_000_000
                    + i64::from_le_bytes(ev[8..16].try_into().unwrap());
                let ty = u16::from_le_bytes(ev[16..18].try_into().unwrap());
                let code = u16::from_le_bytes(ev[18..20].try_into().unwrap());
                let value = i32::from_le_bytes(ev[20..24].try_into().unwrap());
                if ty == 0 && code == 3 {
                    self.dropped = true;
                    self.report.clear();
                } else if ty == 0 && code == 0 {
                    if self.dropped {
                        self.snapshot();
                        self.dropped = false;
                    }
                    self.flush(writer, micros)?;
                } else if !self.dropped
                    && let Some(event) = self.mapping.translate(ty, code, value)
                {
                    self.report.push(event);
                }
            }
        }
    }
}

fn stable_id(identity: &str) -> u32 {
    let hash = identity.bytes().fold(2166136261u32, |hash, b| {
        (hash ^ u32::from(b)).wrapping_mul(16777619)
    });
    FIRST + (hash & 0x3fffffff)
}

// Device readiness wakes immediately; the timeout only drives hotplug scans.
fn wait_for_input(polls: &mut [libc::pollfd], next_scan: Instant) {
    let remaining = next_scan.saturating_duration_since(Instant::now());
    let timeout =
        remaining.as_millis() + u128::from(!remaining.subsec_nanos().is_multiple_of(1_000_000));
    unsafe {
        libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout as i32);
    }
}

/// Watches `/dev/input`, or the e2e harness's fake gamepads in
/// `INNKEEPER_GAMEPAD_TEST_DIR`.
pub fn start(broker: PathBuf) {
    let fake = std::env::var_os("INNKEEPER_GAMEPAD_TEST_DIR").filter(|d| !d.is_empty());
    let synthetic = fake.is_some();
    let dir = fake
        .map(PathBuf::from)
        .unwrap_or_else(|| "/dev/input".into());
    std::thread::spawn(move || {
        let Ok(writer) = Evdev::new(broker) else {
            return;
        };
        let mut controllers: HashMap<PathBuf, Controller> = HashMap::new();
        let mut identities = HashMap::new();
        let mut next_scan = Instant::now();
        loop {
            if Instant::now() >= next_scan {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name
                        .strip_prefix("event")
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                        || controllers.contains_key(&path)
                    {
                        continue;
                    }
                    let Ok(mut controller) = Controller::open(&path, synthetic) else {
                        continue;
                    };
                    if controllers
                        .values()
                        .any(|c| c.identity == controller.identity)
                    {
                        controller
                            .identity
                            .push_str(&format!("@{}", path.display()));
                    }
                    let id = if let Some(id) = identities.get(&controller.identity) {
                        *id
                    } else {
                        let mut id = stable_id(&controller.identity);
                        while identities.values().any(|&v| v == id) {
                            id += 1;
                        }
                        identities.insert(controller.identity.clone(), id);
                        id
                    };
                    controller.id = id;
                    if writer
                        .gamepad(
                            id,
                            &format!("{id:08x}"),
                            controller.feedback.as_ref(),
                            synthetic,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    controller.snapshot();
                    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
                    unsafe {
                        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
                    }
                    let _ = controller.flush(&writer, now.tv_sec * 1_000_000 + now.tv_nsec / 1000);
                    crate::info!("gamepad: connected {} as {id}", controller.identity);
                    controllers.insert(path, controller);
                }
                next_scan = Instant::now() + Duration::from_millis(250);
            }
            let mut polls: Vec<_> = controllers
                .values()
                .map(|c| libc::pollfd {
                    fd: c.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            wait_for_input(&mut polls, next_scan);
            controllers.retain(|_, controller| match controller.read(&writer) {
                Ok(()) => true,
                Err(e) => {
                    let _ = writer.device(controller.id, false);
                    crate::info!("gamepad: removed {}: {e}", controller.identity);
                    false
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_wait_reaches_scan_deadline_but_input_wakes_it() {
        use std::os::unix::net::UnixDatagram;
        let start = Instant::now();
        wait_for_input(&mut [], start + Duration::from_millis(50));
        assert!(start.elapsed() >= Duration::from_millis(50));

        let (input, sender) = UnixDatagram::pair().unwrap();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(b"input").unwrap();
        });
        let mut polls = [libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        wait_for_input(&mut polls, Instant::now() + Duration::from_secs(5));
        assert_eq!(polls[0].revents, libc::POLLIN);
        worker.join().unwrap();
    }

    #[test]
    fn xbox_axes_buttons_and_dpad_follow_android_generic_layout() {
        let mut m = Mapping::new(
            &[
                (0, 0, 65535),
                (1, 0, 65535),
                (2, 0, 255),
                (3, -32768, 32767),
                (4, -32768, 32767),
                (5, 0, 255),
            ],
            0x045e,
        );
        assert_eq!(m.translate(3, 3, 32767), Some((3, 2, 32767)));
        assert_eq!(m.translate(3, 4, -32768), Some((3, 5, -32768)));
        assert_eq!(m.translate(3, 2, 255), Some((3, 10, 32767))); // LTRIGGER
        assert_eq!(m.translate(3, 5, 255), Some((3, 9, 32767))); // RTRIGGER
        assert_eq!(m.translate(3, 0, 0), Some((3, 0, -32768)));
        assert_eq!(m.translate(1, 304, 1), Some((1, 304, 1)));
        assert_eq!(m.translate(1, 546, 1), Some((3, 16, -1)));
        assert_eq!(m.translate(1, 547, 1), Some((3, 16, 0)));
        assert_eq!(m.translate(1, 546, 0), Some((3, 16, 1)));
        assert_eq!(m.translate(1, 30, 1), None);
        let mut generic = Mapping::new(
            &[
                (0, -1, 1),
                (1, -1, 1),
                (2, -32768, 32767),
                (3, 0, 0), // unsupported RX/RY still return zeroed EVIOCGABS
                (4, 0, 0),
                (5, -32768, 32767),
            ],
            0x045e,
        );
        assert_eq!(generic.translate(3, 2, 32767), Some((3, 2, 32767)));
        assert_eq!(generic.translate(3, 5, -32768), Some((3, 5, -32768)));
        assert_eq!(generic.translate(3, 3, 0), None);
        assert_eq!(generic.translate(3, 4, 0), None);
        assert_eq!(generic.translate(1, 312, 1), Some((3, 10, 32767)));
        let mut sony = Mapping::new(
            &[
                (0, -32768, 32767),
                (1, -32768, 32767),
                (2, 0, 255),
                (3, -32768, 32767),
                (4, -32768, 32767),
                (5, 0, 255),
            ],
            0x054c,
        );
        assert_eq!(sony.translate(1, 307, 1), Some((1, 308, 1))); // Triangle -> Y
        assert_eq!(sony.translate(1, 308, 1), Some((1, 307, 1))); // Square -> X
        assert_eq!(sony.translate(3, 2, 128), Some((3, 10, 16447)));
        assert_eq!(sony.translate(1, 312, 1), None); // digital transition cannot replace pressure
        assert_eq!(sony.translate(3, 5, 64), Some((3, 9, 8223)));
        assert_eq!(sony.translate(1, 313, 1), None);
        assert_ne!(stable_id("one"), stable_id("two"));
        assert_eq!(stable_id("one"), stable_id("one"));
    }
}
