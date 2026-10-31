//! `DocumentReader.open_file`: open every path component relative to an
//! anchored directory descriptor with `O_NOFOLLOW`, then `fstat` the final
//! descriptor. The returned fd is what gets read or streamed; the path is
//! never re-opened, so a swapped symlink or renamed directory cannot redirect
//! the read after validation (TOCTOU).
//!
//! Learning note (Python → Rust): `os.open(part, flags, dir_fd=d)` is
//! `openat(2)`; rustix exposes it safely with `OwnedFd`, which closes on
//! drop. That replaces Python's explicit `os.close` in `finally` blocks.

use super::canonical_path;
use crate::fault::{Fault, Result};
use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
use std::os::fd::OwnedFd;
use std::path::Path;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOCTTY);

/// A verified regular file plus the metadata needed for HTTP headers.
pub struct OpenedFile {
    pub fd: OwnedFd,
    pub size: u64,
    pub modified: i64,
}

pub fn open_file(root: &Path, relative: &str, maximum: u64) -> Result<OpenedFile> {
    let relative = canonical_path(relative)?;
    let mut directory = open(root, DIRECTORY, Mode::empty()).map_err(|_| Fault::NotReadable)?;
    let parts: Vec<&str> = relative.split('/').collect();
    let (last, parents) = parts.split_last().ok_or(Fault::NotReadable)?;
    for part in parents {
        directory =
            openat(&directory, *part, DIRECTORY, Mode::empty()).map_err(|_| Fault::NotReadable)?;
    }
    let fd = openat(&directory, *last, FILE, Mode::empty()).map_err(|_| Fault::NotReadable)?;
    let info = fstat(&fd).map_err(|_| Fault::NotReadable)?;
    let size = u64::try_from(info.st_size).unwrap_or(u64::MAX);
    if FileType::from_raw_mode(info.st_mode) != FileType::RegularFile || size > maximum {
        return Err(Fault::NotReadable);
    }
    Ok(OpenedFile {
        fd,
        size,
        modified: info.st_mtime,
    })
}

/// Read at most `limit` bytes from an opened descriptor.
pub fn read_limited(file: OpenedFile, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut raw = Vec::new();
    std::fs::File::from(file.fd)
        .take(limit)
        .read_to_end(&mut raw)
        .map_err(|_| Fault::Os)?;
    Ok(raw)
}
