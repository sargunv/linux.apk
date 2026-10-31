//! Opens a window through a running bridge the way Android does: asks for it
//! on the input service as the window manager does (WINDOW_REQUEST), then adds
//! a display of that size on the display service as the composer does
//! (DISPLAY_ADD, a udmabuf pool, BUFFER_ADD, FRAME, RELEASE). It animates a
//! test pattern and prints the input service's events.
//!
//!   cargo run -p innkeeper-bridge --example window -- SOCKET [SECONDS]

use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use innkeeper_bridge::display::proto::{self as d, BufferSpec, Request, fourcc};
use innkeeper_bridge::input::proto::{self as input, WindowRequest};
use innkeeper_bridge::ipc::proto::{Hello, service};
use innkeeper_bridge::ipc::wire;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage,
    SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

const W: u32 = 640;
const H: u32 = 400;
const WINDOW: u32 = 42;
const DISPLAY: u32 = 7;

struct Conn {
    sock: OwnedFd,
    tag: u32,
    released: Vec<u32>,
    closed: bool,
}

impl Conn {
    fn connect(path: &str, service: u32) -> Result<Conn> {
        let sock = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        rustix::net::connect(&sock, &SocketAddrUnix::new(path)?)?;
        let mut c = Conn {
            sock,
            tag: 1,
            released: vec![],
            closed: false,
        };
        let hello = Hello {
            major: 1,
            minor: 0,
            service,
            name: "window example".into(),
        };
        rustix::net::send(&c.sock, &hello.encode(1), SendFlags::NOSIGNAL)?;
        loop {
            if let Some((_, st)) = c.read(None)? {
                if st != 0 {
                    bail!("HELLO for service {service}: {st}");
                }
                return Ok(c);
            }
        }
    }

    fn handle_event(&mut self, ty: u16, body: &[u8]) {
        let u32_at = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().unwrap());
        match ty {
            d::ty::RELEASE => self.released.push(u32_at(0)),
            input::ty::INPUT => {
                let n = u16::from_le_bytes([body[0], body[1]]) as usize;
                let line = String::from_utf8_lossy(&body[2..2 + n]);
                println!("input: {line}");
                self.closed |= line == format!("close {WINDOW}");
            }
            input::ty::WINDOW_MAPPED => println!("window {} mapped", u32_at(0)),
            input::ty::WINDOW_UNMAPPED => println!("window {} unmapped", u32_at(0)),
            t => println!("event {t:#x}"),
        }
    }

    fn readable(&self, t: Duration) -> Result<bool> {
        let mut p = [rustix::event::PollFd::new(
            &self.sock,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::time::Timespec {
            tv_sec: t.as_secs() as i64,
            tv_nsec: t.subsec_nanos() as i64,
        };
        Ok(rustix::event::poll(&mut p, Some(&ts))? > 0)
    }

    /// Handles the events that are waiting.
    fn drain(&mut self) -> Result<()> {
        while self.readable(Duration::ZERO)? {
            self.read(None)?;
        }
        Ok(())
    }

    /// Reads one packet; events are handled, a reply is returned.
    fn read(&mut self, timeout: Option<Duration>) -> Result<Option<(u32, i32)>> {
        if let Some(t) = timeout
            && !self.readable(t)?
        {
            return Ok(None);
        }
        let mut buf = vec![0u8; 1024];
        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(4))];
        let mut anc = RecvAncillaryBuffer::new(&mut space);
        let m = rustix::net::recvmsg(
            &self.sock,
            &mut [IoSliceMut::new(&mut buf)],
            &mut anc,
            RecvFlags::CMSG_CLOEXEC,
        )?;
        if m.bytes == 0 {
            bail!("bridge closed the connection");
        }
        let h = wire::decode_header(&buf[..m.bytes]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        if h.ty == wire::REPLY {
            let (tag, status, _) = wire::decode_reply(&buf[..m.bytes]).context("reply")?;
            return Ok(Some((tag, status)));
        }
        self.handle_event(h.ty, &buf[wire::HDR..m.bytes]);
        Ok(None)
    }

    fn call(&mut self, encode: impl Fn(u32) -> Vec<u8>, fd: Option<&OwnedFd>) -> Result<i32> {
        self.tag += 1;
        let msg = encode(self.tag);
        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut anc = SendAncillaryBuffer::new(&mut space);
        let fds;
        if let Some(f) = fd {
            fds = [f.as_fd()];
            anc.push(SendAncillaryMessage::ScmRights(&fds));
        }
        rustix::net::sendmsg(
            &self.sock,
            &[std::io::IoSlice::new(&msg)],
            &mut anc,
            SendFlags::NOSIGNAL,
        )?;
        loop {
            if let Some((tag, status)) = self.read(None)?
                && tag == self.tag
            {
                return Ok(status);
            }
        }
    }
}

fn udmabuf(size: usize) -> Result<(OwnedFd, *mut u8)> {
    let memfd = rustix::fs::memfd_create("window-example", rustix::fs::MemfdFlags::ALLOW_SEALING)?;
    rustix::fs::ftruncate(&memfd, size as u64)?;
    rustix::fs::fcntl_add_seals(&memfd, rustix::fs::SealFlags::SHRINK)?;
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            size,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &memfd,
            0,
        )?
    } as *mut u8;
    let dev = rustix::fs::open(
        "/dev/udmabuf",
        rustix::fs::OFlags::RDWR,
        rustix::fs::Mode::empty(),
    )?;
    let mut c = Create {
        memfd: memfd.as_raw_fd() as u32,
        flags: 1, // UDMABUF_FLAGS_CLOEXEC
        offset: 0,
        size: size as u64,
    };
    let fd = unsafe { rustix::ioctl::ioctl(&dev, UdmabufCreate(&mut c))? };
    Ok((fd, map))
}

/// struct udmabuf_create { u32 memfd; u32 flags; u64 offset; u64 size; }
#[repr(C)]
struct Create {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// UDMABUF_CREATE (_IOW('u', 0x42, struct udmabuf_create)); returns the dma-buf.
struct UdmabufCreate<'a>(&'a mut Create);

unsafe impl rustix::ioctl::Ioctl for UdmabufCreate<'_> {
    type Output = OwnedFd;
    const IS_MUTATING: bool = false;
    fn opcode(&self) -> rustix::ioctl::Opcode {
        rustix::ioctl::opcode::write::<Create>(b'u', 0x42)
    }
    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        (self.0 as *mut Create).cast()
    }
    unsafe fn output_from_ptr(
        out: rustix::ioctl::IoctlOutput,
        _: *mut std::ffi::c_void,
    ) -> rustix::io::Result<OwnedFd> {
        use std::os::fd::FromRawFd;
        Ok(unsafe { OwnedFd::from_raw_fd(out) })
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).context("usage: window SOCKET [SECONDS]")?;
    let secs: f64 = args.get(2).map_or(Ok(10.0), |s| s.parse())?;
    let mut wm = Conn::connect(path, service::INPUT)?;
    let mut c = Conn::connect(path, service::DISPLAY)?;
    let request = input::Request::WindowRequest(WindowRequest {
        window: WINDOW,
        width: W,
        height: H,
        dpi: 160,
        min_width: 0,
        min_height: 0,
        app_id: "dev.sargunv.innkeeper.example".into(),
        title: "Bridge window example".into(),
        token: String::new(),
    });
    let st = wm.call(|t| input::encode(t, &request), None)?;
    println!("WINDOW_REQUEST: {st}");
    let add = Request::DisplayAdd {
        display: DISPLAY,
        width: W,
        height: H,
    };
    let st = c.call(|t| d::encode(t, &add), None)?;
    println!("DISPLAY_ADD: {st}");
    let stride = W * 4;
    let size = (stride * H) as usize;
    let mut bufs = vec![];
    for i in 0..3u32 {
        let (fd, map) = udmabuf(size)?;
        let add = Request::BufferAdd(BufferSpec {
            buffer: i,
            display: DISPLAY,
            width: W,
            height: H,
            fourcc: fourcc::XBGR8888,
            stride,
            offset: 0,
            modifier: 0,
        });
        let st = c.call(|t| d::encode(t, &add), Some(&fd))?;
        println!("BUFFER_ADD {i}: {st}");
        bufs.push((map, true));
    }
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed().as_secs_f64() < secs && !wm.closed {
        for r in std::mem::take(&mut c.released) {
            bufs[r as usize].1 = true;
        }
        if let Some(i) = bufs.iter().position(|b| b.1) {
            let px = unsafe { std::slice::from_raw_parts_mut(bufs[i].0, size) };
            for y in 0..H {
                for x in 0..W {
                    let o = ((y * W + x) * 4) as usize;
                    px[o] = (x * 255 / W) as u8;
                    px[o + 1] = (y * 255 / H) as u8;
                    px[o + 2] = (n * 4) as u8;
                    px[o + 3] = 255;
                }
            }
            bufs[i].1 = false;
            let frame = Request::Frame {
                display: DISPLAY,
                buffer: i as u32,
                damage: [0; 4],
            };
            c.call(|t| d::encode(t, &frame), None)?;
            n += 1;
        }
        let _ = c.read(Some(Duration::from_millis(16)))?;
        wm.drain()?;
    }
    println!("{n} frames");
    Ok(())
}
