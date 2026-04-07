//! Windows-specific I/O optimizations.
//!
//! Provides long-path support (\\\\?\\ prefix), `FSCTL_GET_RETRIEVAL_POINTERS`
//! for physical offset resolution on NTFS, `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
//! for block cloning on ReFS, and `CopyFileExW` for kernel-optimized copies.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_INVALID_FUNCTION, FALSE, HANDLE, INVALID_HANDLE_VALUE,
    TRUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileExW, CreateFileW, DeviceIoControl, GetFileInformationByHandle,
    GetVolumeInformationByHandleW, SetFilePointerEx, BY_HANDLE_FILE_INFORMATION,
    CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_RETRIEVAL_POINTERS,
};

/// Maximum path length before we apply long-path prefix.
const MAX_SHORT_PATH: usize = 260;

/// Apply the Windows long-path prefix (`\\?\`) if the path exceeds 260 characters.
///
/// Returns the path unchanged if it's short enough or already prefixed.
#[must_use]
pub fn ensure_long_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();

    // Already has long-path prefix.
    if s.starts_with("\\\\?\\") {
        return path.to_path_buf();
    }

    // UNC paths: \\server\share → \\?\UNC\server\share.
    if s.starts_with("\\\\") {
        if s.len() > MAX_SHORT_PATH {
            let without_leading = &s[2..];
            return PathBuf::from(format!("\\\\?\\UNC\\{without_leading}"));
        }
        return path.to_path_buf();
    }

    // Regular paths: only add prefix if > 260.
    if s.len() > MAX_SHORT_PATH {
        // Canonicalize to absolute path first.
        if let Ok(abs) = std::fs::canonicalize(path) {
            return PathBuf::from(format!("\\\\?\\{}", abs.display()));
        }
        return PathBuf::from(format!("\\\\?\\{s}"));
    }

    path.to_path_buf()
}

/// Convert a Rust `Path` to a wide (UTF-16) null-terminated string for Win32 API calls.
fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Open a file handle using `CreateFileW`.
///
/// # Safety
///
/// The returned handle must be closed with `CloseHandle`.
fn open_file(path: &Path, read: bool) -> Result<HANDLE, io::Error> {
    let wide = to_wide(path);
    let (access, disposition) = if read {
        (FILE_GENERIC_READ, OPEN_EXISTING)
    } else {
        (FILE_GENERIC_WRITE, CREATE_ALWAYS)
    };

    // SAFETY: `CreateFileW` is the standard Win32 file-open function. We pass
    // a valid null-terminated wide string and reasonable flags.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ,
            std::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

/// RAII wrapper for a Win32 `HANDLE`.
struct WinHandle(HANDLE);

impl Drop for WinHandle {
    fn drop(&mut self) {
        // SAFETY: We only store valid handles obtained from `CreateFileW`.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Get the physical disk offset of a file's first cluster on NTFS.
///
/// Uses `FSCTL_GET_RETRIEVAL_POINTERS` to query the volume's cluster map.
/// Returns `None` if the file system doesn't support this operation.
///
/// # Errors
///
/// Returns an `io::Error` if the file cannot be opened.
pub fn get_ntfs_physical_offset(path: &Path) -> Result<Option<u64>, io::Error> {
    let handle = open_file(path, true)?;
    let _guard = WinHandle(handle);

    // Input: STARTING_VCN_INPUT_BUFFER — start from VCN 0.
    #[repr(C)]
    struct StartingVcnInput {
        starting_vcn: i64,
    }
    let input = StartingVcnInput { starting_vcn: 0 };

    // Output: RETRIEVAL_POINTERS_BUFFER header + 1 extent.
    #[repr(C)]
    struct RetrievalPointersBuffer {
        extent_count: u32,
        _padding: u32,
        starting_vcn: i64,
        // First extent.
        next_vcn: i64,
        lcn: i64,
    }
    let mut output = RetrievalPointersBuffer {
        extent_count: 0,
        _padding: 0,
        starting_vcn: 0,
        next_vcn: 0,
        lcn: 0,
    };
    let mut bytes_returned: u32 = 0;

    // SAFETY: `DeviceIoControl` with `FSCTL_GET_RETRIEVAL_POINTERS` reads the
    // cluster map. We provide properly sized input/output buffers.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_RETRIEVAL_POINTERS,
            std::ptr::addr_of!(input).cast(),
            std::mem::size_of::<StartingVcnInput>()
                .try_into()
                .unwrap_or(u32::MAX),
            std::ptr::addr_of_mut!(output).cast(),
            std::mem::size_of::<RetrievalPointersBuffer>()
                .try_into()
                .unwrap_or(u32::MAX),
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if ok == FALSE {
        let err = unsafe { GetLastError() };
        if err == ERROR_INVALID_FUNCTION {
            // File system doesn't support retrieval pointers.
            return Ok(None);
        }
        return Err(io::Error::from_raw_os_error(err.try_into().unwrap_or(0)));
    }

    if output.extent_count > 0 && output.lcn >= 0 {
        Ok(Some(output.lcn.cast_unsigned()))
    } else {
        Ok(None)
    }
}

/// Duplicate extent structure for `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
#[repr(C)]
struct DuplicateExtentsData {
    file_handle: HANDLE,
    source_file_offset: i64,
    target_file_offset: i64,
    byte_count: i64,
}

/// Attempt ReFS block cloning via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
///
/// Returns `Ok(true)` if the clone succeeded, `Ok(false)` if the file system
/// doesn't support block cloning (e.g. NTFS).
///
/// # Errors
///
/// Returns an `io::Error` if file operations fail for reasons other than
/// unsupported file system operations.
pub fn try_refs_block_clone(src: &Path, dest: &Path) -> Result<bool, io::Error> {
    let src_handle = open_file(src, true)?;
    let _src_guard = WinHandle(src_handle);

    // Get source file size.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `GetFileInformationByHandle` reads file metadata from a valid handle.
    let ok = unsafe { GetFileInformationByHandle(src_handle, &mut info) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    let file_size =
        i64::from(info.nFileSizeHigh) << 32 | i64::from(info.nFileSizeLow);

    if file_size == 0 {
        // Empty files don't need block cloning.
        return Ok(false);
    }

    let dest_handle = open_file(dest, false)?;
    let _dest_guard = WinHandle(dest_handle);

    // Set destination file size to match source.
    // SAFETY: `SetFilePointerEx` sets the file pointer position on a valid handle.
    let ok = unsafe {
        SetFilePointerEx(dest_handle, file_size, std::ptr::null_mut(), FILE_BEGIN)
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `SetEndOfFile` truncates/extends the file at the current pointer.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::SetEndOfFile(dest_handle) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    let dup_data = DuplicateExtentsData {
        file_handle: src_handle,
        source_file_offset: 0,
        target_file_offset: 0,
        byte_count: file_size,
    };
    let mut bytes_returned: u32 = 0;

    // SAFETY: `DeviceIoControl` with `FSCTL_DUPLICATE_EXTENTS_TO_FILE` performs
    // block-level cloning. We provide a valid source handle and proper offsets.
    let ok = unsafe {
        DeviceIoControl(
            dest_handle,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            std::ptr::addr_of!(dup_data).cast(),
            std::mem::size_of::<DuplicateExtentsData>()
                .try_into()
                .unwrap_or(u32::MAX),
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if ok == FALSE {
        let err = unsafe { GetLastError() };
        if err == ERROR_INVALID_FUNCTION {
            debug!("ReFS block clone not supported, falling back");
            return Ok(false);
        }
        return Err(io::Error::from_raw_os_error(err.try_into().unwrap_or(0)));
    }

    debug!(
        "ReFS block clone succeeded: {} -> {}",
        src.display(),
        dest.display()
    );
    Ok(true)
}

/// Copy a file using `CopyFileExW` (kernel-optimized copy with progress).
///
/// Returns `Ok(true)` if the copy succeeded, `Ok(false)` on failure.
///
/// # Errors
///
/// Returns an `io::Error` if the copy fails for an unexpected reason.
pub fn try_copy_file_ex(src: &Path, dest: &Path) -> Result<bool, io::Error> {
    let wide_src = to_wide(src);
    let wide_dest = to_wide(dest);

    // SAFETY: `CopyFileExW` is the standard Win32 file-copy function. We pass
    // valid null-terminated wide strings and no cancel flag.
    let ok = unsafe {
        CopyFileExW(
            wide_src.as_ptr(),
            wide_dest.as_ptr(),
            None,            // no progress callback
            std::ptr::null(),
            std::ptr::null_mut(), // no cancel flag
            0,               // no flags — overwrite if exists
        )
    };

    if ok == FALSE {
        let err = io::Error::last_os_error();
        warn!("CopyFileExW failed: {err}");
        Ok(false)
    } else {
        Ok(true)
    }
}

/// Try the optimal Windows copy chain:
/// 1. ReFS block clone (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`)
/// 2. `CopyFileExW` (kernel-optimized)
/// 3. Return `Ok(false)` for buffered fallback
///
/// # Errors
///
/// Returns an `io::Error` if file operations fail unexpectedly.
pub fn try_windows_optimal_copy(src: &Path, dest: &Path) -> Result<bool, io::Error> {
    // Try ReFS block clone first.
    match try_refs_block_clone(src, dest) {
        Ok(true) => return Ok(true),
        Ok(false) => {}
        Err(e) => {
            debug!("ReFS block clone attempt failed: {e}");
        }
    }

    // Try CopyFileExW.
    match try_copy_file_ex(src, dest) {
        Ok(true) => return Ok(true),
        Ok(false) => {}
        Err(e) => {
            debug!("CopyFileExW attempt failed: {e}");
        }
    }

    Ok(false)
}

/// Get available disk space on Windows using `GetDiskFreeSpaceExW`.
///
/// # Errors
///
/// Returns an `io::Error` if the volume information cannot be queried.
pub fn get_available_space_windows(path: &Path) -> Result<u64, io::Error> {
    let wide = to_wide(path);
    let mut free_bytes_available: u64 = 0;

    // SAFETY: `GetDiskFreeSpaceExW` queries volume capacity. We pass a valid
    // null-terminated wide path.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    if ok == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(free_bytes_available)
    }
}

/// Detect the file system type for a volume (e.g. "NTFS", "ReFS").
///
/// Returns `None` if the file system name cannot be determined.
///
/// # Errors
///
/// Returns an `io::Error` if the file cannot be opened.
pub fn detect_filesystem(path: &Path) -> Result<Option<String>, io::Error> {
    let handle = open_file(path, true)?;
    let _guard = WinHandle(handle);

    let mut fs_name: [u16; 256] = [0; 256];

    // SAFETY: `GetVolumeInformationByHandleW` queries volume metadata from a
    // valid file handle.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(), // volume name — not needed
            0,
            std::ptr::null_mut(), // serial number
            std::ptr::null_mut(), // max component length
            std::ptr::null_mut(), // file system flags
            fs_name.as_mut_ptr(),
            fs_name.len().try_into().unwrap_or(u32::MAX),
        )
    };

    if ok == FALSE {
        return Ok(None);
    }

    // Find null terminator.
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..len]);
    if name.is_empty() {
        Ok(None)
    } else {
        Ok(Some(name))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn long_path_short_unchanged() {
        let p = Path::new("C:\\Users\\test\\file.txt");
        assert_eq!(ensure_long_path(p), p);
    }

    #[test]
    fn long_path_already_prefixed_unchanged() {
        let p = Path::new("\\\\?\\C:\\very\\long\\path");
        assert_eq!(ensure_long_path(p), p);
    }

    #[test]
    fn long_path_exceeding_max_gets_prefix() {
        // Build a path > 260 chars.
        let long_component = "a".repeat(300);
        let p = PathBuf::from(format!("C:\\{long_component}"));
        let result = ensure_long_path(&p);
        let s = result.to_string_lossy();
        assert!(
            s.starts_with("\\\\?\\"),
            "expected long-path prefix, got: {s}"
        );
    }

    #[test]
    fn long_path_unc_exceeding_max_gets_prefix() {
        let long_component = "a".repeat(300);
        let p = PathBuf::from(format!("\\\\server\\{long_component}"));
        let result = ensure_long_path(&p);
        let s = result.to_string_lossy();
        assert!(
            s.starts_with("\\\\?\\UNC\\"),
            "expected UNC long-path prefix, got: {s}"
        );
    }

    #[test]
    fn long_path_unc_short_unchanged() {
        let p = Path::new("\\\\server\\share\\file.txt");
        assert_eq!(ensure_long_path(p), p);
    }

    #[test]
    fn try_windows_optimal_copy_nonexistent_returns_error() {
        let result = try_windows_optimal_copy(
            Path::new("C:\\nonexistent\\source.bin"),
            Path::new("C:\\nonexistent\\dest.bin"),
        );
        // Should fail since source doesn't exist.
        assert!(result.is_err());
    }

    #[test]
    fn detect_filesystem_on_c_drive() {
        // C:\ should be NTFS or ReFS on most Windows systems.
        let result = detect_filesystem(Path::new("C:\\"));
        match result {
            Ok(Some(fs)) => {
                assert!(
                    fs == "NTFS" || fs == "ReFS",
                    "unexpected filesystem: {fs}"
                );
            }
            Ok(None) => {} // Can happen in CI environments.
            Err(_) => {}   // May fail in restricted environments.
        }
    }

    #[test]
    fn get_available_space_c_drive() {
        let result = get_available_space_windows(Path::new("C:\\"));
        match result {
            Ok(space) => assert!(space > 0, "expected non-zero free space"),
            Err(_) => {} // May fail in CI or containerized environments.
        }
    }
}
