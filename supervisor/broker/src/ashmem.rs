//! Ashmem for OpenHarmony (`INNKEEPER_GUEST=ohos`): OH's kernel keeps the
//! staging ashmem driver, and its c_utils (`AshmemCreate`) and everything on
//! it (IPC parcels, the display HDI's shared command queues) need
//! `/dev/ashmem`. The host has none, so each open of it gets a fresh sealable
//! memfd, and the broker answers the ashmem ioctls on those memfds:
//!
//! - the size is the memfd's: SET_SIZE truncates, GET_SIZE is its length;
//! - a protection mask without `PROT_WRITE` is `F_SEAL_FUTURE_WRITE` (no new
//!   writable mappings, as ashmem refuses them), and masks only shrink;
//! - names, pinning and purging are accepted and have no effect: memfd pages
//!   are never purged, so every region stays pinned.
//!
//! The state lives in the memfd itself, so a region passed to another process
//! answers the same there. c_utils also checks that the opened node is a
//! character device: the broker's fstat ([`crate::fs`]) shows a region as
//! [`RDEV`].

use std::fs::File;
use std::os::fd::AsRawFd;

pub const NODE: &[u8] = b"/dev/ashmem";
/// `__ASHMEMIOC`.
pub const IOCTL_TYPE: u8 = 0x77;
/// What fstat shows as a region's device: misc 10:61, as ashmem was.
pub const RDEV: u64 = 10 << 8 | 61;
/// The memfd name, which marks the broker's regions (`/memfd:ashmem`).
pub const NAME: &std::ffi::CStr = c"ashmem";

// linux/ashmem.h, by number.
const SET_NAME: u32 = 1;
const GET_NAME: u32 = 2;
const SET_SIZE: u32 = 3;
const GET_SIZE: u32 = 4;
const SET_PROT_MASK: u32 = 5;
const GET_PROT_MASK: u32 = 6;
const PIN: u32 = 7;
const UNPIN: u32 = 8;
const GET_PIN_STATUS: u32 = 9;
const PURGE_ALL_CACHES: u32 = 10;
const NAME_LEN: usize = 256;
const IS_PINNED: i64 = 1;
const ALL_PROT: u64 = (libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC) as u64;

/// Is `cmd` an ashmem ioctl (by its type byte)?
pub fn is_ashmem_ioctl(cmd: u32) -> bool {
    (cmd >> 8) & 0xff == IOCTL_TYPE as u32
}

/// Is `link` (an fd's `/proc` link) one of the broker's regions?
pub fn is_region(link: &[u8]) -> bool {
    link.strip_prefix(b"/memfd:")
        .is_some_and(|n| n.starts_with(NAME.to_bytes()))
}

/// A new region for an open of [`NODE`].
pub fn create() -> std::io::Result<File> {
    let fd =
        unsafe { libc::memfd_create(NAME.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) })
}

fn seals(f: &File) -> Result<i32, i32> {
    match unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GET_SEALS) } {
        -1 => Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)),
        s => Ok(s),
    }
}

fn prot(f: &File) -> Result<u64, i32> {
    Ok(if seals(f)? & libc::F_SEAL_FUTURE_WRITE != 0 {
        ALL_PROT & !(libc::PROT_WRITE as u64)
    } else {
        ALL_PROT
    })
}

/// Serves `cmd` on region `f`. `arg` is the ioctl's argument; `write` stores
/// bytes at the pointer it names. Returns the ioctl's value or an errno.
pub fn ioctl(
    f: &File,
    cmd: u32,
    arg: u64,
    write: &mut dyn FnMut(&[u8]) -> bool,
) -> Result<i64, i32> {
    match cmd & 0xff {
        SET_NAME | PIN | UNPIN | PURGE_ALL_CACHES => Ok(0),
        GET_NAME => {
            let mut name = [0u8; NAME_LEN];
            name[..NAME.to_bytes().len()].copy_from_slice(NAME.to_bytes());
            if write(&name) {
                Ok(0)
            } else {
                Err(libc::EFAULT)
            }
        }
        SET_SIZE => {
            let size = i64::try_from(arg).map_err(|_| libc::EINVAL)?;
            f.set_len(size as u64)
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
            Ok(0)
        }
        GET_SIZE => Ok(f
            .metadata()
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?
            .len() as i64),
        SET_PROT_MASK => {
            if arg & !prot(f)? != 0 {
                return Err(libc::EINVAL);
            }
            if arg & libc::PROT_WRITE as u64 == 0
                && unsafe {
                    libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_FUTURE_WRITE)
                } != 0
            {
                return Err(std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO));
            }
            Ok(0)
        }
        GET_PROT_MASK => Ok(prot(f)? as i64),
        GET_PIN_STATUS => Ok(IS_PINNED),
        _ => Err(libc::ENOTTY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &[u8]) -> bool {
        false
    }

    #[test]
    fn size_and_protection_live_in_the_memfd() {
        let f = create().unwrap();
        assert_eq!(ioctl(&f, 0x40087703, 8192, &mut none), Ok(0));
        assert_eq!(ioctl(&f, 0x7704, 0, &mut none), Ok(8192));
        assert_eq!(ioctl(&f, 0x7706, 0, &mut none), Ok(ALL_PROT as i64));
        let read = libc::PROT_READ as u64;
        assert_eq!(ioctl(&f, 0x40087705, read, &mut none), Ok(0));
        assert_eq!(ioctl(&f, 0x7706, 0, &mut none), Ok(ALL_PROT as i64 & !2));
        // A mask only shrinks, and no new mapping may write.
        assert_eq!(
            ioctl(&f, 0x40087705, ALL_PROT, &mut none),
            Err(libc::EINVAL)
        );
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                8192,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert_eq!(p, libc::MAP_FAILED);
    }

    #[test]
    fn regions_are_named_memfds() {
        assert!(is_region(b"/memfd:ashmem (deleted)"));
        assert!(!is_region(b"/memfd:innkeeper-binder-3 (deleted)"));
        assert!(!is_region(b"/dev/ashmem"));
        let mut name = Vec::new();
        let f = create().unwrap();
        assert_eq!(
            ioctl(&f, 0x81007702, 0, &mut |b| {
                name = b.to_vec();
                true
            }),
            Ok(0)
        );
        assert!(name.starts_with(b"ashmem\0"));
        assert_eq!(ioctl(&f, 0x7709, 0, &mut none), Ok(IS_PINNED));
        assert_eq!(ioctl(&f, 0x770b, 0, &mut none), Err(libc::ENOTTY));
    }
}
