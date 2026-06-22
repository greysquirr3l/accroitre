//! Linux-specific I/O optimizations for zero-copy file operations.
//!
//! Uses `copy_file_range(2)` for kernel-to-kernel large file copies,
//! `splice(2)` for pipe-based zero-copy transfers, and `sendfile(2)` for
//! file-to-socket zero-copy. All functions fall back gracefully when the
//! syscall is unavailable or returns an unsupported-operation error.

#![cfg(target_os = "linux")]

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::domain::CopyError;

/// Maximum single `copy_file_range` call size (128 MiB).
const CFR_CHUNK_SIZE: usize = 128 * 1024 * 1024;

/// Maximum single `sendfile` call size (128 MiB).
const SENDFILE_CHUNK_SIZE: usize = 128 * 1024 * 1024;

/// Copy a file using `copy_file_range(2)`.
///
/// The kernel copies data directly between file descriptors without
/// passing through userspace. Falls back with `Ok(false)` if the
/// syscall is not supported (ENOSYS, EXDEV, EOPNOTSUPP, EINVAL).
pub fn try_copy_file_range(src: &Path, dest: &Path) -> Result<bool, CopyError> {
    let src_file = fs::File::open(src).map_err(|e| CopyError::FileCopy {
        src: src.to_path_buf(),
        dst: dest.to_path_buf(),
        source: e,
    })?;

    let dest_file = fs::File::create(dest).map_err(|e| CopyError::FileCopy {
        src: src.to_path_buf(),
        dst: dest.to_path_buf(),
        source: e,
    })?;

    let file_size = src_file
        .metadata()
        .map_err(|e| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: e,
        })?
        .len();

    let src_fd = src_file.as_raw_fd();
    let dest_fd = dest_file.as_raw_fd();
    let mut offset_in: i64 = 0;
    let mut offset_out: i64 = 0;
    let mut remaining = file_size;

    while remaining > 0 {
        let chunk = if remaining > CFR_CHUNK_SIZE as u64 {
            CFR_CHUNK_SIZE
        } else {
            remaining as usize
        };

        // SAFETY: `copy_file_range` is a Linux syscall. We pass valid file
        // descriptors and properly initialised offset pointers.
        let n = unsafe {
            libc::copy_file_range(
                src_fd,
                &raw mut offset_in,
                dest_fd,
                &raw mut offset_out,
                chunk,
                0,
            )
        };

        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                // Unsupported — caller should fall back to buffered copy.
                Some(libc::ENOSYS | libc::EXDEV | libc::EOPNOTSUPP | libc::EINVAL) => {
                    debug!(
                        "copy_file_range not supported for {} -> {}: {err}",
                        src.display(),
                        dest.display()
                    );
                    // Clean up partial dest file.
                    let _ = fs::remove_file(dest);
                    Ok(false)
                }
                _ => Err(CopyError::FileCopy {
                    src: src.to_path_buf(),
                    dst: dest.to_path_buf(),
                    source: err,
                }),
            };
        }

        if n == 0 {
            break; // EOF
        }

        remaining -= n as u64;
    }

    debug!(
        "copy_file_range: {} -> {} ({file_size} bytes)",
        src.display(),
        dest.display()
    );
    Ok(true)
}

/// Send file data to a raw file descriptor using `sendfile(2)`.
///
/// Useful for file-to-socket zero-copy transfers. Returns the number of
/// bytes sent, or `Ok(None)` if the syscall is unavailable.
///
/// # Errors
///
/// Returns a `CopyError` if the syscall fails with an error other than
/// ENOSYS or EOPNOTSUPP.
pub fn try_sendfile(src: &Path, dest_fd: i32) -> Result<Option<u64>, CopyError> {
    let src_file = fs::File::open(src).map_err(|e| CopyError::FileCopy {
        src: src.to_path_buf(),
        dst: Path::new("<socket>").to_path_buf(),
        source: e,
    })?;

    let file_size = src_file
        .metadata()
        .map_err(|e| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: Path::new("<socket>").to_path_buf(),
            source: e,
        })?
        .len();

    let src_fd = src_file.as_raw_fd();
    let mut offset: i64 = 0;
    let mut total_sent: u64 = 0;

    while total_sent < file_size {
        let remaining = file_size - total_sent;
        let chunk = if remaining > SENDFILE_CHUNK_SIZE as u64 {
            SENDFILE_CHUNK_SIZE
        } else {
            remaining as usize
        };

        // SAFETY: `sendfile` is a Linux syscall. We pass valid file
        // descriptors and a properly initialised offset pointer.
        let n = unsafe { libc::sendfile(dest_fd, src_fd, &raw mut offset, chunk) };

        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL) => {
                    debug!("sendfile not supported: {err}");
                    Ok(None)
                }
                _ => Err(CopyError::FileCopy {
                    src: src.to_path_buf(),
                    dst: Path::new("<socket>").to_path_buf(),
                    source: err,
                }),
            };
        }

        if n == 0 {
            break;
        }

        total_sent += n as u64;
    }

    debug!("sendfile: {} ({total_sent} bytes)", src.display());
    Ok(Some(total_sent))
}

/// Zero-copy transfer through a pipe using `splice(2)`.
///
/// Splices data from `src_fd` to `dest_fd` via a kernel pipe, avoiding
/// copies into userspace. Returns `Ok(true)` on success, `Ok(false)` if
/// the syscall is unsupported.
///
/// # Errors
///
/// Returns a `CopyError` on I/O failure.
pub fn try_splice(src_fd: i32, dest_fd: i32, len: u64) -> Result<bool, CopyError> {
    let mut pipe_fds = [0i32; 2];

    // SAFETY: pipe2 creates a pipe pair. We check the return value.
    let ret = unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret < 0 {
        return Err(CopyError::Transport {
            message: "pipe2() syscall failed".to_owned(),
            path: PathBuf::new(),
            source: io::Error::last_os_error(),
        });
    }

    let pipe_read = pipe_fds[0];
    let pipe_write = pipe_fds[1];

    let result = splice_loop(src_fd, dest_fd, pipe_read, pipe_write, len);

    // SAFETY: closing valid file descriptors.
    unsafe {
        libc::close(pipe_read);
        libc::close(pipe_write);
    }

    result
}

/// Inner splice loop between src_fd → pipe → dest_fd.
fn splice_loop(
    src_fd: i32,
    dest_fd: i32,
    pipe_read: i32,
    pipe_write: i32,
    len: u64,
) -> Result<bool, CopyError> {
    let mut remaining = len;
    let flags = libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE;

    while remaining > 0 {
        let chunk = remaining.min(CFR_CHUNK_SIZE as u64) as usize;

        // Splice from source into pipe.
        // SAFETY: splice is a Linux syscall. We pass valid fds.
        let to_pipe = unsafe {
            libc::splice(
                src_fd,
                std::ptr::null_mut(),
                pipe_write,
                std::ptr::null_mut(),
                chunk,
                flags as u32,
            )
        };

        if to_pipe < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENOSYS | libc::EINVAL) => Ok(false),
                _ => Err(CopyError::Transport {
                    message: "splice(src → pipe) failed".to_owned(),
                    path: PathBuf::new(),
                    source: err,
                }),
            };
        }

        if to_pipe == 0 {
            break;
        }

        // Splice from pipe into destination.
        let mut pipe_remaining = to_pipe;
        while pipe_remaining > 0 {
            // SAFETY: splice is a Linux syscall with valid fds.
            let from_pipe = unsafe {
                libc::splice(
                    pipe_read,
                    std::ptr::null_mut(),
                    dest_fd,
                    std::ptr::null_mut(),
                    pipe_remaining as usize,
                    flags as u32,
                )
            };

            if from_pipe < 0 {
                return Err(CopyError::Transport {
                    message: "splice(pipe → dst) failed".to_owned(),
                    path: PathBuf::new(),
                    source: io::Error::last_os_error(),
                });
            }

            if from_pipe == 0 {
                break;
            }

            pipe_remaining -= from_pipe;
        }

        remaining -= to_pipe as u64;
    }

    Ok(true)
}

/// Detect the running kernel version and return `(major, minor)`.
///
/// Used for runtime capability detection (e.g., io_uring requires 5.1+).
#[must_use]
pub fn kernel_version() -> Option<(u32, u32)> {
    // SAFETY: uname writes into a stack-allocated struct. We check return.
    unsafe {
        let mut utsname: libc::utsname = std::mem::zeroed();
        if libc::uname(&raw mut utsname) != 0 {
            return None;
        }

        let release = std::ffi::CStr::from_ptr(utsname.release.as_ptr());
        let release_str = release.to_str().ok()?;

        parse_kernel_version(release_str)
    }
}

/// Parse a kernel version string like "5.15.0-generic" into (major, minor).
fn parse_kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Check whether io_uring is likely supported (kernel >= 5.1).
#[must_use]
pub fn io_uring_supported() -> bool {
    kernel_version().is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kernel_version_typical() {
        assert_eq!(parse_kernel_version("5.15.0-91-generic"), Some((5, 15)));
        assert_eq!(parse_kernel_version("6.1.0"), Some((6, 1)));
        assert_eq!(parse_kernel_version("4.19.128"), Some((4, 19)));
    }

    #[test]
    fn parse_kernel_version_edge_cases() {
        assert_eq!(parse_kernel_version(""), None);
        assert_eq!(parse_kernel_version("5"), None);
        assert_eq!(parse_kernel_version("abc.def"), None);
    }

    #[test]
    fn io_uring_version_check() {
        // 5.1+ should support io_uring.
        assert!(
            parse_kernel_version("5.1.0")
                .is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 1))
        );
        assert!(
            parse_kernel_version("6.0.0")
                .is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 1))
        );
        // 4.x should not.
        assert!(
            !parse_kernel_version("4.19.0")
                .is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 1))
        );
    }

    #[test]
    fn copy_file_range_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("source.bin");
        let dst = tmp.path().join("dest.bin");

        let data: Vec<u8> = (0..=255).cycle().take(1024 * 1024).collect();
        fs::write(&src, &data)?;

        match try_copy_file_range(&src, &dst)? {
            true => {
                let copied = fs::read(&dst)?;
                assert_eq!(data, copied, "copy_file_range output mismatch");
            }
            false => {
                // Acceptable: kernel/fs doesn't support copy_file_range.
            }
        }
        Ok(())
    }

    #[test]
    fn copy_file_range_empty_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("empty.bin");
        let dst = tmp.path().join("empty_dest.bin");

        fs::write(&src, b"")?;

        match try_copy_file_range(&src, &dst)? {
            true => {
                let copied = fs::read(&dst)?;
                assert!(copied.is_empty());
            }
            false => {}
        }
        Ok(())
    }
}
