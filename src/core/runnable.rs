//! Which files the "Run / Run in Terminal / Open" prompt is for. A regular
//! file is *runnable* when the current user may execute it **and** its
//! content starts like a program: the ELF magic or `#!`. The second test
//! matters because vfat, ntfs and MTP mount every file as executable, and a
//! prompt on every document there would be noise.
//!
//! A `.desktop` file is a launcher, not a program: it opens in the text
//! editor and has its own "Launch" row (see [`is_desktop_file`]).

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use rustix::fs::{Access, OFlags};

/// The ELF magic: the first four bytes of a native executable.
const ELF_MAGIC: &[u8] = b"\x7fELF";

/// The first two bytes of a script with an interpreter line.
const SHEBANG: &[u8] = b"#!";

/// Whether `bytes`, the start of a file, begin like a program.
#[must_use]
pub fn starts_like_a_program(bytes: &[u8]) -> bool {
    bytes.starts_with(ELF_MAGIC) || bytes.starts_with(SHEBANG)
}

/// Whether `path` is a regular file (a symlink is resolved, as the default
/// open does) that the current user may execute and whose content starts
/// like a program ([`starts_like_a_program`]).
///
/// It reads at most four bytes. Blocking: the access check and the read may
/// wait on a network or FUSE mount, so call it off the UI thread. The file is
/// opened `O_NONBLOCK` and checked on the descriptor, so a FIFO or device
/// swapped in after the listing cannot stall it. `O_NOFOLLOW` is left out on
/// purpose: a link to a program is a program, exactly as `xdg-open` sees it.
#[must_use]
pub fn is_runnable(path: &Path) -> bool {
    if rustix::fs::access(path, Access::EXEC_OK).is_err() {
        return false;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed() | OFlags::NOCTTY.bits().cast_signed())
        .open(path)
    else {
        return false;
    };
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let mut head = Vec::with_capacity(ELF_MAGIC.len());
    if file
        .take(ELF_MAGIC.len() as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return false;
    }
    starts_like_a_program(&head)
}

/// Whether `path` names a Desktop Entry: a non-empty stem and the
/// `.desktop` extension.
#[must_use]
pub fn is_desktop_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("desktop")) && path.file_stem().is_some()
}
