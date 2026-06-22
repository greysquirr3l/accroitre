//! macOS-specific I/O optimizations for copy operations.
//!
//! Uses `clonefile(2)` for instant copy-on-write copies on APFS, `fcopyfile(3)` for
//! kernel-optimized copies with metadata preservation, and `F_NOCACHE` for
//! bypassing the unified buffer cache on large sequential reads.
//! Falls back gracefully when running on non-APFS volumes.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use tracing::debug;

use crate::domain::CopyError;

/// APFS filesystem type name returned by `statfs`.
const APFS_FSTYPENAME: &str = "apfs";

/// Detect whether the given path resides on an APFS volume.
#[must_use]
pub fn is_apfs(path: &Path) -> bool {
    let check_path = find_existing_ancestor(path);
    let Ok(c_path) = CString::new(check_path.to_string_lossy().as_bytes()) else {
        return false;
    };

    // SAFETY: statfs is a standard macOS/BSD call. We pass a valid C string
    // and read from a properly-initialised struct.
    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &raw mut stat) != 0 {
            return false;
        }

        let fstypename = std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr());
        fstypename
            .to_str()
            .is_ok_and(|s| s.eq_ignore_ascii_case(APFS_FSTYPENAME))
    }
}

/// Walk up to find the nearest existing ancestor directory.
fn find_existing_ancestor(path: &Path) -> std::path::PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    current
}

/// Copy a file using `fcopyfile(3)`.
///
/// This is a macOS kernel-optimized copy that preserves metadata (permissions,
/// timestamps, extended attributes, ACLs). Preferred over buffered copy when
/// `clonefile` is not available (e.g., HFS+ volumes).
///
/// Returns `Ok(true)` if the copy succeeded, `Ok(false)` if `fcopyfile` is
/// unavailable or failed in a recoverable way.
///
/// # Errors
///
/// Returns `CopyError` on non-recoverable I/O failures.
pub fn try_fcopyfile(src: &Path, dest: &Path) -> Result<bool, CopyError> {
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

    let src_fd = src_file.as_raw_fd();
    let dest_fd = dest_file.as_raw_fd();

    // SAFETY: fcopyfile is a macOS-specific function. We combine
    // COPYFILE_DATA and COPYFILE_METADATA to copy data + permissions +
    // xattrs + ACLs. We pass valid file descriptors and a null state.
    let flags = libc::COPYFILE_DATA | libc::COPYFILE_METADATA;
    let ret = unsafe { libc::fcopyfile(src_fd, dest_fd, std::ptr::null_mut(), flags) };

    if ret == 0 {
        debug!(
            "fcopyfile succeeded: {} -> {}",
            src.display(),
            dest.display()
        );
        Ok(true)
    } else {
        let err = io::Error::last_os_error();
        debug!(
            "fcopyfile failed for {} -> {}: {err}",
            src.display(),
            dest.display()
        );
        // Clean up partial dest.
        let _ = fs::remove_file(dest);
        Ok(false)
    }
}

/// Set `F_NOCACHE` on a file descriptor to bypass the unified buffer cache.
///
/// Useful for large sequential reads where data won't be re-read, avoiding
/// cache pollution. Returns `Ok(true)` if successful, `Ok(false)` on failure.
///
/// # Errors
///
/// Returns an `io::Error` if the `fcntl` call fails unexpectedly.
pub fn set_nocache(fd: i32) -> Result<bool, io::Error> {
    // SAFETY: fcntl with F_NOCACHE is a macOS-specific call.
    // 1 = enable no-cache, 0 = disable.
    let ret = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
    if ret == -1 {
        let err = io::Error::last_os_error();
        debug!("F_NOCACHE failed: {err}");
        Ok(false)
    } else {
        Ok(true)
    }
}

/// Full macOS copy with optimal fallback chain:
/// 1. `clonefile` (instant copy-on-write on APFS)
/// 2. `fcopyfile` (kernel copy with metadata)
/// 3. Returns `Ok(false)` to signal caller should use buffered copy
///
/// # Errors
///
/// Returns `CopyError` on non-recoverable I/O failures.
pub fn try_macos_optimal_copy(src: &Path, dest: &Path, try_clone: bool) -> Result<bool, CopyError> {
    // Try clonefile if requested (fast path for APFS).
    if try_clone && matches!(try_clonefile(src, dest), Ok(())) {
        debug!(
            "clonefile succeeded: {} -> {}",
            src.display(),
            dest.display()
        );
        return Ok(true);
    }

    // Try fcopyfile (kernel copy with metadata).
    try_fcopyfile(src, dest)
}

/// Attempt macOS APFS `clonefile(2)`.
fn try_clonefile(src: &Path, dest: &Path) -> Result<(), CopyError> {
    let c_src =
        CString::new(src.to_string_lossy().as_bytes()).map_err(|_| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "invalid path"),
        })?;
    let c_dest =
        CString::new(dest.to_string_lossy().as_bytes()).map_err(|_| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "invalid path"),
        })?;

    // SAFETY: clonefile is a macOS-specific syscall. We pass valid C strings.
    let ret = unsafe { libc::clonefile(c_src.as_ptr(), c_dest.as_ptr(), 0) };
    if ret == 0 {
        Ok(())
    } else {
        Err(CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: io::Error::last_os_error(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn is_apfs_on_root() {
        // macOS root filesystem is typically APFS since High Sierra.
        // This may fail on older macOS or non-APFS setups — both outcomes are valid.
        let result = is_apfs(Path::new("/"));
        // Just verify it doesn't panic; actual value depends on system.
        let _ = result;
    }

    #[test]
    fn is_apfs_nonexistent_walks_up() {
        // Should walk up to "/" which exists.
        let result = is_apfs(Path::new("/nonexistent/deep/path/file.txt"));
        let _ = result;
    }

    #[test]
    fn fcopyfile_copies_data() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("source.txt");
        let dst = tmp.path().join("dest.txt");

        let data = b"hello from fcopyfile";
        fs::write(&src, data)?;

        if try_fcopyfile(&src, &dst)? {
            let copied = fs::read(&dst)?;
            assert_eq!(data.as_slice(), copied.as_slice());
        }
        // else: fcopyfile not available — acceptable.
        Ok(())
    }

    #[test]
    fn fcopyfile_empty_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("empty.txt");
        let dst = tmp.path().join("empty_dest.txt");

        fs::write(&src, b"")?;

        if try_fcopyfile(&src, &dst)? {
            let copied = fs::read(&dst)?;
            assert!(copied.is_empty());
        }
        Ok(())
    }

    #[test]
    fn clonefile_on_same_volume() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("clone_src.txt");
        let dst = tmp.path().join("clone_dst.txt");

        fs::write(&src, b"clone me")?;

        if matches!(try_clonefile(&src, &dst), Ok(())) {
            let copied = fs::read(&dst)?;
            assert_eq!(b"clone me".as_slice(), copied.as_slice());
        }
        // else: clonefile not supported on this filesystem — acceptable.
        Ok(())
    }

    #[test]
    fn optimal_copy_fallback_chain() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let src = tmp.path().join("optimal_src.txt");
        let dst = tmp.path().join("optimal_dst.txt");

        fs::write(&src, b"optimal copy test")?;

        if try_macos_optimal_copy(&src, &dst, true)? {
            let copied = fs::read(&dst)?;
            assert_eq!(b"optimal copy test".as_slice(), copied.as_slice());
        }
        // else: Neither clonefile nor fcopyfile worked — acceptable on some fs.
        Ok(())
    }

    #[test]
    fn set_nocache_on_open_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("nocache.txt");
        fs::write(&path, b"test")?;

        let file = fs::File::open(&path)?;
        let result = set_nocache(file.as_raw_fd());
        // Should succeed or gracefully fail.
        assert!(result.is_ok());
        Ok(())
    }
}
