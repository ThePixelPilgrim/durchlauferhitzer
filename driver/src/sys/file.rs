//! Files beneath a static root: `openat2`, `fstat`, `readahead`,
//! `sendfile`, and the errno-to-[`FileError`] table of the contract.

use std::ffi::CString;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use durchlauferhitzer_core::{FileError, FileKind};

use super::fd::{Io, cvt_retry, errno_is, transfer};

/// The open flags of the file contract: `O_RDONLY | O_NONBLOCK | O_NOCTTY |
/// O_CLOEXEC` (`O_NONBLOCK` so a FIFO cannot hang the opening worker).
pub const OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;

/// The resolve flags of every open beneath a root.
const RESOLVE: u64 = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;

/// Whether the kernel has `openat2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Openat2 {
    Supported,
    /// `ENOSYS`.
    Unsupported,
}

/// What `fstat` says about an open descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: FileKind,
    /// `st_size`.
    pub size: u64,
    /// `st_mtime` in nanoseconds since the epoch; before 1970 clamps to 0,
    /// an overflow saturates.
    pub mtime_ns: u64,
}

/// Opens `segments`, joined with `/`, beneath `dir` with `openat2` and
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`; an empty list opens `"."` (the
/// directory itself). The segments are not checked here: the kernel refuses
/// `..`, absolute paths and symlinks (`EXDEV`, `ELOOP`). A NUL byte in a
/// segment is `InvalidInput`. `flags` is normally [`OPEN_FLAGS`].
pub fn openat2_beneath<S: AsRef<[u8]>>(
    dir: BorrowedFd<'_>,
    segments: &[S],
    flags: libc::c_int,
) -> io::Result<OwnedFd> {
    let mut path = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            path.push(b'/');
        }
        path.extend_from_slice(segment.as_ref());
    }
    if path.is_empty() {
        path.push(b'.');
    }
    let path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in a path"))?;
    openat2(dir, &path, flags, RESOLVE)
}

fn openat2(
    dir: BorrowedFd<'_>,
    path: &CString,
    flags: libc::c_int,
    resolve: u64,
) -> io::Result<OwnedFd> {
    // SAFETY: an all-zero `open_how` is a valid value (no mode, no flags).
    let mut how: libc::open_how = unsafe { zeroed() };
    // The flags are a bit pattern: reinterpret, never sign-extend.
    how.flags = u64::from(flags as u32);
    how.resolve = resolve;
    let fd = cvt_retry(|| {
        // SAFETY: `path` is a NUL-terminated string and `how` a valid
        // `open_how` of the size passed; the kernel only reads both.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir.as_raw_fd(),
                path.as_ptr(),
                &raw const how,
                size_of::<libc::open_how>(),
            )
        };
        // A descriptor or -1: always fits a c_int.
        libc::c_int::try_from(ret).unwrap_or(-1)
    })?;
    // SAFETY: `fd` was just created by `openat2` and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Probes for `openat2` by opening `"."` beneath `dir` (as an `O_PATH`
/// descriptor, so no read permission is needed). `ENOSYS` is
/// `Unsupported`; any other error is returned.
pub fn openat2_supported(dir: BorrowedFd<'_>) -> io::Result<Openat2> {
    let dot = c".".to_owned();
    match openat2(dir, &dot, libc::O_PATH | libc::O_CLOEXEC, RESOLVE) {
        Ok(_) => Ok(Openat2::Supported),
        Err(err) if errno_is(&err, libc::ENOSYS) => Ok(Openat2::Unsupported),
        Err(err) => Err(err),
    }
}

/// Opens a directory as an `O_PATH | O_DIRECTORY | O_CLOEXEC` descriptor,
/// for a static root.
pub fn open_dir_path(path: &Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in a path"))?;
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let fd = cvt_retry(|| {
        // SAFETY: `path` is a NUL-terminated string the kernel only reads.
        unsafe { libc::open(path.as_ptr(), flags) }
    })?;
    // SAFETY: `fd` was just created by `open` and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fstat` of an open descriptor.
pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    // SAFETY: an all-zero `stat` is a valid value.
    let mut st: libc::stat = unsafe { zeroed() };
    // SAFETY: `st` is a writable `stat`.
    cvt_retry(|| unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => FileKind::Regular,
        libc::S_IFDIR => FileKind::Directory,
        _ => FileKind::Other,
    };
    Ok(Stat {
        kind,
        size: u64::try_from(st.st_size).unwrap_or(0),
        mtime_ns: mtime_ns(st.st_mtime, st.st_mtime_nsec),
    })
}

/// `sec * 10^9 + nsec`, clamped to 0 before 1970 and saturating above.
fn mtime_ns(sec: i64, nsec: i64) -> u64 {
    let total = i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    u64::try_from(total.max(0)).unwrap_or(u64::MAX)
}

/// Asks the kernel to read `len` bytes at `offset` into the page cache
/// (`readahead(2)`); it may block on the disk.
pub fn readahead(fd: BorrowedFd<'_>, offset: u64, len: u64) -> io::Result<()> {
    let offset = libc::off64_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    cvt_retry(|| {
        // SAFETY: `readahead` takes no pointers.
        let ret = unsafe { libc::readahead(fd.as_raw_fd(), offset, len) };
        // 0 or -1.
        libc::c_int::try_from(ret).unwrap_or(-1)
    })?;
    Ok(())
}

/// Sends up to `len` bytes of `file` from `offset` to the socket `out`
/// (`sendfile`, leaving the file's own offset alone). `Closed` means the
/// file ended before `offset` (a 0 return: truncated); `len` 0 is `Done(0)`
/// without a system call. A non-blocking `out` gives `WouldBlock`.
pub fn sendfile(out: BorrowedFd<'_>, file: BorrowedFd<'_>, offset: u64, len: u64) -> Io {
    if len == 0 {
        return Io::Done(0);
    }
    let Ok(mut off) = libc::off_t::try_from(offset) else {
        return Io::Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offset too large",
        ));
    };
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    transfer(|| {
        // SAFETY: `off` is a writable `off_t` the kernel advances.
        unsafe { libc::sendfile(out.as_raw_fd(), file.as_raw_fd(), &mut off, len) }
    })
}

/// Maps an open or read error to a [`FileError`] by the contract's table.
pub fn file_error(err: &io::Error) -> FileError {
    match err.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR) => FileError::NotFound,
        Some(libc::ELOOP | libc::EXDEV) => FileError::Symlink,
        Some(libc::EACCES | libc::EPERM) => FileError::PermissionDenied,
        Some(_) | None => FileError::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::mtime_ns;

    #[test]
    fn mtime_clamps_before_1970_and_saturates() {
        assert_eq!(mtime_ns(0, 0), 0);
        assert_eq!(mtime_ns(1, 5), 1_000_000_005);
        assert_eq!(mtime_ns(-1, 999_999_999), 0);
        assert_eq!(mtime_ns(-1, 0), 0);
        assert_eq!(mtime_ns(i64::MIN, 0), 0);
        assert_eq!(mtime_ns(i64::MAX, 999_999_999), u64::MAX);
        assert_eq!(mtime_ns(18_446_744_073, 709_551_615), u64::MAX);
        assert_eq!(mtime_ns(18_446_744_073, 709_551_616), u64::MAX);
        assert_eq!(mtime_ns(18_446_744_073, 709_551_614), u64::MAX - 1);
    }
}
