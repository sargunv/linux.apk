//! Canonical evdev layout consumed by Android's Generic.kl. The bridge maps
//! host axes to this layout; FF_RUMBLE retains the Linux ff_effect ABI.
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub const FIRST: u32 = 0x10000;
pub const AXES: [u16; 8] = [0, 1, 2, 5, 9, 10, 16, 17];
pub const BUTTONS: [u16; 11] = [304, 305, 307, 308, 314, 316, 315, 317, 318, 310, 311];
pub const EFFECT_SIZE: usize = 48;

pub fn absinfo(code: u16, value: i32) -> Option<Vec<u8>> {
    if !AXES.contains(&code) {
        return None;
    }
    let (min, max, flat) = match code {
        9 | 10 => (0, 32767, 0),
        16 | 17 => (-1, 1, 0),
        _ => (-32768, 32767, 4096),
    };
    Some(
        [value, min, max, 0, flat, 0]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect(),
    )
}

pub struct Feedback {
    fd: OwnedFd,
    // Synthetic host sources: the e2e harness's fake gamepads.
    synthetic: bool,
    // Effects belong to this host open and the virtual endpoint that uploaded
    // them. They cannot outlive or move to a replacement host fd.
    effects: HashMap<RawFd, Vec<i16>>,
}
impl Feedback {
    pub fn new(fd: OwnedFd, synthetic: bool) -> Self {
        Self {
            fd,
            synthetic,
            effects: HashMap::new(),
        }
    }

    pub fn ioctl(
        &mut self,
        client: RawFd,
        cmd: u32,
        bytes: &mut [u8],
        scalar: u64,
    ) -> Result<i64, i32> {
        let result = self.host_ioctl(cmd, bytes, scalar)?;
        match cmd & 0xff {
            0x80 => {
                let id = i16::from_le_bytes(bytes[2..4].try_into().unwrap());
                let effects = self.effects.entry(client).or_default();
                if !effects.contains(&id) {
                    effects.push(id);
                }
            }
            0x81 => {
                if let Some(effects) = self.effects.get_mut(&client) {
                    effects.retain(|&id| i64::from(id) != scalar as i64);
                }
            }
            _ => {}
        }
        Ok(result)
    }

    pub fn close_client(&mut self, client: RawFd) {
        if let Some(effects) = self.effects.remove(&client) {
            for id in effects {
                if let Err(e) = self.host_ioctl(0x40044581, &mut [], id as u64) {
                    eprintln!("evdev: rumble cleanup: {e}");
                }
            }
        }
    }

    fn host_ioctl(&self, cmd: u32, bytes: &mut [u8], scalar: u64) -> Result<i64, i32> {
        let n = if self.synthetic {
            let mut request = cmd.to_le_bytes().to_vec();
            request.extend_from_slice(&scalar.to_le_bytes());
            request.extend_from_slice(bytes);
            let sent = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    request.as_ptr().cast(),
                    request.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if sent != request.len() as isize {
                return Err(libc::EIO);
            }
            let mut reply = vec![0; 4 + bytes.len()];
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    reply.as_mut_ptr().cast(),
                    reply.len(),
                    0,
                )
            };
            if n != reply.len() as isize {
                return Err(libc::EIO);
            }
            bytes.copy_from_slice(&reply[4..]);
            let result = i32::from_le_bytes(reply[..4].try_into().unwrap());
            if result < 0 {
                return Err(-result);
            }
            result
        } else {
            let arg = if bytes.is_empty() {
                scalar as libc::c_ulong
            } else {
                bytes.as_mut_ptr() as libc::c_ulong
            };
            unsafe { libc::ioctl(self.fd.as_raw_fd(), cmd as _, arg) }
        };
        if n < 0 {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        } else {
            Ok(i64::from(n))
        }
    }

    pub fn write(&self, events: &[u8]) -> std::io::Result<()> {
        let n = if self.synthetic {
            let mut packet = 0u32.to_le_bytes().to_vec();
            packet.extend_from_slice(events);
            unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_NOSIGNAL,
                )
            }
        } else {
            unsafe { libc::write(self.fd.as_raw_fd(), events.as_ptr().cast(), events.len()) }
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        while let Some(&client) = self.effects.keys().next() {
            self.close_client(client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    #[test]
    fn rumble_upload_erase_and_idle_playback_reach_the_source() {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) },
            0
        );
        let feedback = Feedback::new(unsafe { OwnedFd::from_raw_fd(fds[0]) }, true);
        let source = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let worker = std::thread::spawn(move || {
            let mut bytes = [0; 128];
            let n = unsafe {
                libc::recv(
                    source.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            assert_eq!(n, 12 + EFFECT_SIZE as isize);
            assert_eq!(&bytes[..4], &0x40304580u32.to_le_bytes());
            assert_eq!(&bytes[12 + 16..12 + 20], &[0x34, 0x12, 0xcd, 0xab]);
            let mut response = vec![0; 4];
            response.extend_from_slice(&bytes[12..n as usize]);
            response[6..8].copy_from_slice(&7i16.to_le_bytes());
            unsafe {
                libc::send(
                    source.as_raw_fd(),
                    response.as_ptr().cast(),
                    response.len(),
                    0,
                );
            }
            let n = unsafe {
                libc::recv(
                    source.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            assert_eq!(n, 28);
            assert_eq!(&bytes[4..28], &crate::input::event(1, 21, 7, 1));
            let n = unsafe {
                libc::recv(
                    source.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            assert_eq!(n, 12);
            assert_eq!(&bytes[..4], &0x40044581u32.to_le_bytes());
            assert_eq!(&bytes[4..12], &7u64.to_le_bytes());
            let error = (-libc::EINVAL).to_le_bytes();
            unsafe {
                libc::send(source.as_raw_fd(), error.as_ptr().cast(), error.len(), 0);
            }
        });
        let mut effect = [0; EFFECT_SIZE];
        effect[..2].copy_from_slice(&0x50u16.to_le_bytes());
        effect[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        effect[16..20].copy_from_slice(&[0x34, 0x12, 0xcd, 0xab]);
        assert_eq!(feedback.host_ioctl(0x40304580, &mut effect, 0), Ok(0));
        assert_eq!(&effect[2..4], &7i16.to_le_bytes());
        feedback.write(&crate::input::event(1, 21, 7, 1)).unwrap();
        assert_eq!(
            feedback.host_ioctl(0x40044581, &mut [], 7),
            Err(libc::EINVAL)
        );
        worker.join().unwrap();
    }
}
