//! End-to-end tests of the bridge over a real socket, with the null output:
//! a well-behaved client streams through the ring, and hostile clients
//! (garbage, oversized packets, attached fds, resource exhaustion) are
//! answered or dropped without disturbing anyone else.

use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use innkeeper_bridge::audio::proto::{
    self as audio, OpenReply, Request, SampleFormat, StreamConfig,
};
use innkeeper_bridge::audio::{AudioService, Backend, Limits};
use innkeeper_bridge::ipc::proto::{Hello, hello_reply_body, service};
use innkeeper_bridge::ipc::server::{self, Config, Events, Registry, Reply, Service};
use innkeeper_bridge::ipc::transport;
use innkeeper_bridge::ipc::wire::{self, errno::*};

use rustix::event::{PollFd, PollFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketAddrUnix,
    SocketFlags, SocketType,
};

static N: AtomicU32 = AtomicU32::new(0);

/// A test service: request 0x0d01 echoes its body back as event 0x4001.
const ECHO: u32 = 13;
struct Echo(Events);
impl Service for Echo {
    fn handle(&mut self, ty: u16, body: &[u8], _fds: Vec<OwnedFd>) -> Reply {
        match ty {
            0x0d01 => match self.0.try_send(0x4001, body, &[]) {
                Ok(()) => Reply::ok(),
                Err(_) => Reply::err(-EIO),
            },
            // Events outside the event range are refused by the sender.
            0x0d02 => match self.0.try_send(0x0101, body, &[]) {
                Ok(()) => Reply::ok(),
                Err(_) => Reply::err(-EINVAL),
            },
            _ => Reply::err(-ENOSYS),
        }
    }
}

/// A test service that greets each connection with event 0x4002.
const GREET: u32 = 14;
struct Greet(Events);
impl Service for Greet {
    fn handle(&mut self, _: u16, _: &[u8], _: Vec<OwnedFd>) -> Reply {
        Reply::err(-ENOSYS)
    }

    fn connected(&mut self) {
        self.0.try_send(0x4002, b"hi", &[]).unwrap();
    }
}

fn start(limits: Limits) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "bridge-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let l = transport::listen(&path).unwrap();
    let mut services = Registry::default();
    let streams = Arc::new(AtomicUsize::new(0));
    services.register(service::AUDIO, "audio", move |peer, _events| {
        Ok(Box::new(AudioService::new(
            Backend::Null,
            limits.clone(),
            streams.clone(),
            peer,
        )))
    });
    services.register(ECHO, "echo", |_peer, events| Ok(Box::new(Echo(events))));
    services.register(GREET, "greet", |_peer, events| Ok(Box::new(Greet(events))));
    let cfg = Config {
        max_clients: 4,
        services,
    };
    std::thread::spawn(move || server::serve(l, Arc::new(cfg)));
    path
}

fn hello(major: u16, service: u32) -> Hello {
    Hello {
        major,
        minor: 0,
        service,
        name: "test".into(),
    }
}

struct Client {
    sock: OwnedFd,
    tag: u32,
}

impl Client {
    fn connect(path: &PathBuf) -> Client {
        let sock = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        rustix::net::connect(&sock, &SocketAddrUnix::new(path).unwrap()).unwrap();
        Client { sock, tag: 0 }
    }

    fn send_raw(&self, b: &[u8], fds: &[OwnedFd]) -> anyhow::Result<()> {
        transport::send_owned(&self.sock, b, fds)
    }

    /// Receives one packet: None on EOF or timeout.
    fn recv(&self) -> Option<(Vec<u8>, Vec<OwnedFd>)> {
        let mut pfd = [PollFd::new(&self.sock, PollFlags::IN)];
        let t = rustix::time::Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        if rustix::event::poll(&mut pfd, Some(&t)).unwrap() == 0 {
            return None;
        }
        let mut buf = vec![0u8; 1024];
        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let m = rustix::net::recvmsg(
            &self.sock,
            &mut [IoSliceMut::new(&mut buf)],
            &mut anc,
            RecvFlags::CMSG_CLOEXEC,
        )
        .ok()?;
        let mut fds = Vec::new();
        for a in anc.drain() {
            if let RecvAncillaryMessage::ScmRights(f) = a {
                fds.extend(f);
            }
        }
        if m.bytes == 0 {
            return None;
        }
        buf.truncate(m.bytes);
        Some((buf, fds))
    }

    /// Sends a message built by `f(tag)` and returns (status, body, fds).
    fn call_with(
        &mut self,
        f: impl FnOnce(u32) -> Vec<u8>,
    ) -> Option<(i32, Vec<u8>, Vec<OwnedFd>)> {
        self.tag += 1;
        self.send_raw(&f(self.tag), &[]).ok()?;
        let (b, fds) = self.recv()?;
        let (tag, status, body) = wire::decode_reply(&b).unwrap();
        assert_eq!(tag, self.tag);
        Some((status, body.to_vec(), fds))
    }

    fn call(&mut self, req: &Request) -> Option<(i32, Vec<u8>, Vec<OwnedFd>)> {
        self.call_with(|t| audio::encode(t, req))
    }

    fn hello(&mut self) {
        let r = self
            .call_with(|t| hello(1, service::AUDIO).encode(t))
            .unwrap();
        assert_eq!(r.0, 0);
        assert_eq!(r.1, hello_reply_body());
    }
}

fn out_cfg(period: u32, periods: u32) -> Request {
    Request::OpenOutput(StreamConfig {
        rate: 48000,
        channels: 2,
        format: SampleFormat::S16Le,
        period_frames: period,
        periods,
        name: "test".into(),
    })
}

fn wait_readable(fd: &OwnedFd, ms: i64) -> bool {
    let mut pfd = [PollFd::new(fd, PollFlags::IN)];
    let t = rustix::time::Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    rustix::event::poll(&mut pfd, Some(&t)).unwrap() > 0
}

#[test]
fn stream_through_ring() {
    let path = start(Limits::default());
    let mut c = Client::connect(&path);
    c.hello();
    let (status, body, fds) = c.call(&out_cfg(480, 4)).unwrap();
    assert_eq!(status, 0);
    let r = OpenReply::decode(&body).unwrap();
    assert_eq!((r.capacity_frames, r.data_offset), (1920, 4096));
    assert_eq!(fds.len(), 3);
    let [memfd, data, space]: [OwnedFd; 3] = fds.try_into().unwrap();
    // SAFETY: test mapping of the ring we were given, of its advertised size.
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            r.map_size as usize,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &memfd,
            0,
        )
        .unwrap()
    } as *mut u8;
    // SAFETY: header fields are 8-aligned inside the first page.
    let hdr = |off: usize| unsafe { &*(map.add(off) as *const std::sync::atomic::AtomicU64) };
    assert_eq!(unsafe { *(map as *const u32) }, 0x5241_584c);
    // The client cannot shrink the ring under the bridge.
    assert!(rustix::fs::ftruncate(&memfd, 0).is_err());

    // Write 100 ms of frames, a period at a time, and watch them drain at
    // roughly real time.
    let t0 = Instant::now();
    let mut written = 0u64;
    while written < 4800 {
        let read = hdr(128).load(Ordering::Acquire);
        if written - read + 480 <= 1920 {
            written += 480;
            hdr(64).store(written, Ordering::Release);
            rustix::io::write(&data, &1u64.to_ne_bytes()).unwrap();
        } else {
            assert!(wait_readable(&space, 1000), "no space signal");
            let mut b = [0u8; 8];
            let _ = rustix::io::read(&space, &mut b);
        }
    }
    while hdr(128).load(Ordering::Acquire) < written {
        assert!(wait_readable(&space, 1000));
        let mut b = [0u8; 8];
        let _ = rustix::io::read(&space, &mut b);
    }
    let el = t0.elapsed();
    assert!(
        el >= Duration::from_millis(60) && el < Duration::from_millis(400),
        "{el:?}"
    );
    // Timing was published.
    assert!(hdr(208).load(Ordering::Acquire) > 0);

    assert_eq!(c.call(&Request::Standby { stream: r.stream }).unwrap().0, 0);
    assert_eq!(
        c.call(&Request::SetVolume {
            stream: r.stream,
            left: 0.5,
            right: 0.5
        })
        .unwrap()
        .0,
        0
    );
    assert_eq!(
        c.call(&Request::SetMaster {
            volume: 1.0,
            muted: true
        })
        .unwrap()
        .0,
        0
    );
    assert_eq!(
        c.call(&Request::SetMetadata {
            stream: r.stream,
            usage: 14,
            label: String::new()
        })
        .unwrap()
        .0,
        0
    );
    assert_eq!(c.call(&Request::Close { stream: r.stream }).unwrap().0, 0);
    assert_eq!(
        c.call(&Request::Close { stream: r.stream }).unwrap().0,
        -ENOENT
    );
    // The ring is marked dead once the stream is gone.
    assert_eq!(unsafe { *(map.add(224) as *const u32) }, 2);
}

/// A client that makes its signalling fds blocking and saturates them can't
/// hang the stream thread, so closing the stream still answers.
#[test]
fn stream_fds_cannot_block_the_bridge() {
    let path = start(Limits::default());
    let mut c = Client::connect(&path);
    c.hello();
    let (status, body, fds) = c.call(&out_cfg(480, 4)).unwrap();
    assert_eq!(status, 0);
    let r = OpenReply::decode(&body).unwrap();
    let [memfd, data, space]: [OwnedFd; 3] = fds.try_into().unwrap();
    for fd in [&data, &space] {
        rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::empty()).unwrap();
    }
    // Saturate the space signal, if it is a counter we can write.
    let _ = rustix::io::write(&space, &(u64::MAX - 1).to_ne_bytes());
    // Queue a full ring so the stream consumes and signals space.
    // SAFETY: test mapping of the ring we were given, of its advertised size.
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            r.map_size as usize,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &memfd,
            0,
        )
        .unwrap()
    } as *mut u8;
    // SAFETY: header fields are 8-aligned inside the first page.
    let write_pos = unsafe { &*(map.add(64) as *const std::sync::atomic::AtomicU64) };
    write_pos.store(1920, Ordering::Release);
    rustix::io::write(&data, &1u64.to_ne_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let closed = c.call(&Request::Close { stream: r.stream });
    assert_eq!(closed.map(|r| r.0), Some(0), "close hung");
}

#[test]
fn protocol_errors() {
    let path = start(Limits::default());
    // Requests before HELLO are refused and the connection dropped.
    let mut c = Client::connect(&path);
    assert_eq!(c.call(&Request::Close { stream: 1 }).unwrap().0, -EPROTO);
    assert!(c.recv().is_none());
    // Wrong major, unknown service, malformed HELLO.
    let mut c = Client::connect(&path);
    assert_eq!(
        c.call_with(|t| hello(2, service::AUDIO).encode(t))
            .unwrap()
            .0,
        -EPROTO
    );
    let mut c = Client::connect(&path);
    assert_eq!(c.call_with(|t| hello(1, 99).encode(t)).unwrap().0, -ENOSYS);
    let mut c = Client::connect(&path);
    assert_eq!(
        c.call_with(|t| hello(1, service::DISPLAY).encode(t))
            .unwrap()
            .0,
        -ENOSYS
    );
    let mut c = Client::connect(&path);
    assert_eq!(
        c.call_with(|t| {
            let mut b = hello(1, 1).encode(t);
            b.push(0);
            b
        })
        .unwrap()
        .0,
        -EINVAL
    );

    let mut c = Client::connect(&path);
    c.hello();
    // Second HELLO, event types, bad values, unknown type, unknown stream:
    // answered.
    assert_eq!(
        c.call_with(|t| hello(1, service::AUDIO).encode(t))
            .unwrap()
            .0,
        -EPROTO
    );
    assert_eq!(
        c.call_with(|t| wire::Writer::message(0x4001, t).into_vec())
            .unwrap()
            .0,
        -EPROTO
    );
    assert_eq!(c.call(&out_cfg(1, 4)).unwrap().0, -EINVAL);
    assert_eq!(c.call(&Request::Standby { stream: 42 }).unwrap().0, -ENOENT);
    assert_eq!(
        c.call_with(|t| wire::Writer::message(0x0155, t).into_vec())
            .unwrap()
            .0,
        -ENOSYS
    );
    // Attached fds are closed and the request refused.
    let fd = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC).unwrap();
    c.send_raw(&audio::encode(78, &Request::Standby { stream: 1 }), &[fd])
        .unwrap();
    let (b, fds) = c.recv().unwrap();
    assert!(fds.is_empty());
    assert_eq!(wire::decode_reply(&b).unwrap().1, -EPROTO);
    // Oversized packet: dropped.
    c.send_raw(&[0u8; 2000], &[]).unwrap();
    assert!(c.recv().is_none());

    // The server still serves others.
    let mut d = Client::connect(&path);
    d.hello();
}

#[test]
fn resource_caps() {
    let limits = Limits {
        streams_per_client: 2,
        ring_bytes_per_client: 1 << 20,
        streams_total: 3,
    };
    let path = start(limits);
    let mut a = Client::connect(&path);
    a.hello();
    assert_eq!(a.call(&out_cfg(480, 2)).unwrap().0, 0);
    assert_eq!(a.call(&out_cfg(480, 2)).unwrap().0, 0);
    assert_eq!(a.call(&out_cfg(480, 2)).unwrap().0, -EBUSY); // per client
    let mut b = Client::connect(&path);
    b.hello();
    // 16384 * 16 frames * 4 bytes = 1 MiB + header: over the byte budget.
    assert_eq!(b.call(&out_cfg(16384, 16)).unwrap().0, -EBUSY);
    assert_eq!(b.call(&out_cfg(480, 2)).unwrap().0, 0);
    assert_eq!(b.call(&out_cfg(480, 2)).unwrap().0, -EBUSY); // global
    // Closing a's connection frees its streams.
    drop(a);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(b.call(&out_cfg(480, 2)).unwrap().0, 0);
    // Client cap: 4 connections at most.
    let extra: Vec<Client> = (0..4).map(|_| Client::connect(&path)).collect();
    std::thread::sleep(Duration::from_millis(200));
    let mut last = Client::connect(&path);
    assert!(last.call_with(|t| hello(1, 1).encode(t)).is_none());
    drop(extra);
}

#[test]
fn listen_refuses_to_replace_files() {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("not-a-socket-{}", std::process::id()));
    std::fs::write(&p, b"keep me").unwrap();
    assert!(transport::listen(&p).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), b"keep me");
    std::fs::remove_file(&p).unwrap();
}

#[test]
fn service_events() {
    let path = start(Limits::default());
    let mut c = Client::connect(&path);
    assert_eq!(c.call_with(|t| hello(1, ECHO).encode(t)).unwrap().0, 0);
    c.send_raw(
        &{
            let mut w = wire::Writer::message(0x0d01, 5);
            w.u32(0xfeed);
            w.into_vec()
        },
        &[],
    )
    .unwrap();
    // The event (tag 0) and the reply (tag 5) both arrive, in either order.
    let mut got = Vec::new();
    for _ in 0..2 {
        let (b, _) = c.recv().unwrap();
        let h = wire::decode_header(&b).unwrap();
        got.push((h.ty, h.tag, b[wire::HDR..].to_vec()));
    }
    got.sort();
    assert_eq!(got[0], (0x4001, 0, 0xfeedu32.to_le_bytes().to_vec()));
    assert_eq!((got[1].0, got[1].1), (wire::REPLY, 5));
    c.tag = 5;
    assert_eq!(
        c.call_with(|t| wire::Writer::message(0x0d02, t).into_vec())
            .unwrap()
            .0,
        -EINVAL
    );
}

#[test]
fn connected_events_follow_the_hello_reply() {
    let path = start(Limits::default());
    let mut c = Client::connect(&path);
    // call_with reads the next message as the reply: the greeting can't come first.
    assert_eq!(c.call_with(|t| hello(1, GREET).encode(t)).unwrap().0, 0);
    let (b, _) = c.recv().unwrap();
    assert_eq!(wire::decode_header(&b).unwrap().ty, 0x4002);
    assert_eq!(&b[wire::HDR..], b"hi");
}
