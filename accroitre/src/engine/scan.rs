//! Directory tree scanner with physical offset resolution.

use std::path::{Path, PathBuf};

use glob::Pattern;
use tokio::fs;
use tracing::{debug, warn};

use crate::domain::{FileEntry, ScanError};
use crate::ports::{ProgressPort, ProgressUpdate};

/// Configuration for a directory scan.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Glob patterns for files/directories to exclude.
    pub exclude_patterns: Vec<Pattern>,
    /// Whether to follow symbolic links (default: true).
    pub follow_symlinks: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            exclude_patterns: Vec::new(),
            follow_symlinks: true,
        }
    }
}

/// Result of a directory scan.
#[derive(Debug)]
pub struct ScanResult {
    /// Discovered file entries, sorted by physical offset where available.
    pub entries: Vec<FileEntry>,
    /// Errors encountered during scanning (non-fatal).
    pub errors: Vec<ScanError>,
}

/// Scan a directory tree and return all file entries.
///
/// Walks the tree recursively, collecting metadata for each regular file.
/// Errors on individual files are collected (non-fatal) — the scan continues.
/// After collection, entries are sorted by physical offset for sequential I/O.
///
/// # Errors
///
/// Returns `ScanError::SourceNotFound` if the root path does not exist.
pub async fn scan_tree(
    root: &Path,
    config: &ScanConfig,
    progress: &dyn ProgressPort,
) -> Result<ScanResult, ScanError> {
    if !root.exists() {
        return Err(ScanError::SourceNotFound(root.to_path_buf()));
    }

    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let mut dirs_to_visit = vec![root.to_path_buf()];

    while let Some(dir) = dirs_to_visit.pop() {
        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => {
                let err = ScanError::ReadDir {
                    path: dir.clone(),
                    source: e,
                };
                warn!("{err}");
                errors.push(err);
                continue;
            }
        };

        loop {
            let dir_entry = match read_dir.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    let err = ScanError::ReadDir {
                        path: dir.clone(),
                        source: e,
                    };
                    warn!("{err}");
                    errors.push(err);
                    continue;
                }
            };

            let path = dir_entry.path();

            // Check exclude patterns.
            if is_excluded(&path, root, &config.exclude_patterns) {
                debug!("excluded: {}", path.display());
                continue;
            }

            // Get metadata — follow symlinks or not.
            let metadata = if config.follow_symlinks {
                match fs::metadata(&path).await {
                    Ok(m) => m,
                    Err(e) => {
                        let err = ScanError::Metadata {
                            path: path.clone(),
                            source: e,
                        };
                        warn!("{err}");
                        errors.push(err);
                        continue;
                    }
                }
            } else {
                match fs::symlink_metadata(&path).await {
                    Ok(m) => m,
                    Err(e) => {
                        let err = ScanError::Metadata {
                            path: path.clone(),
                            source: e,
                        };
                        warn!("{err}");
                        errors.push(err);
                        continue;
                    }
                }
            };

            if metadata.is_dir() {
                dirs_to_visit.push(path);
            } else if metadata.is_file() {
                let permissions = get_permissions(&metadata);
                let modified_epoch = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                let mut entry = FileEntry::new(path, metadata.len());
                entry.permissions = permissions;
                entry.modified_epoch = modified_epoch;
                entries.push(entry);

                progress.update(&ProgressUpdate::ScanProgress {
                    files_found: entries.len() as u64,
                    current_dir: &dir,
                });
            }
            // Skip symlinks when not following, special files, etc.
        }
    }

    // Resolve physical offsets on supported platforms.
    resolve_physical_offsets(&mut entries, &mut errors).await;

    // Sort by physical offset for sequential I/O (files without offsets sort last).
    entries.sort_by_key(|e| e.physical_offset.unwrap_or(u64::MAX));

    Ok(ScanResult { entries, errors })
}

/// Check if a path matches any exclude pattern.
fn is_excluded(path: &Path, root: &Path, patterns: &[Pattern]) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let rel_str = relative.to_string_lossy();

    patterns.iter().any(|p| {
        p.matches(&rel_str)
            || path
                .file_name()
                .is_some_and(|name| p.matches(&name.to_string_lossy()))
    })
}

/// Extract POSIX permission bits from metadata.
#[cfg(unix)]
fn get_permissions(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn get_permissions(_metadata: &std::fs::Metadata) -> u32 {
    0
}

/// Resolve physical disk offsets for each entry.
///
/// On macOS: uses `fcntl(F_LOG2PHYS)` to get the physical block offset.
/// On Linux: uses `FIEMAP` ioctl to get the physical extent offset.
/// On other platforms: leaves offsets as `None`.
async fn resolve_physical_offsets(entries: &mut [FileEntry], errors: &mut Vec<ScanError>) {
    for entry in entries.iter_mut() {
        match get_physical_offset(&entry.path).await {
            Ok(offset) => entry.physical_offset = offset,
            Err(e) => {
                debug!(
                    "could not get physical offset for {}: {e}",
                    entry.path.display()
                );
                errors.push(e);
            }
        }
    }
}

/// Get the physical disk offset for a file.
#[cfg(target_os = "macos")]
async fn get_physical_offset(path: &Path) -> Result<Option<u64>, ScanError> {
    use std::os::unix::io::AsRawFd;

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path).map_err(|e| ScanError::PhysicalOffset {
            path: path.clone(),
            source: e,
        })?;

        // F_LOG2PHYS returns the physical byte offset of the file's first byte.
        // SAFETY: F_LOG2PHYS is a valid fcntl command on macOS. The struct is
        // properly initialized before the syscall.
        let mut log2phys = libc::log2phys {
            l2p_flags: 0,
            l2p_contigbytes: 0,
            l2p_devoffset: 0,
        };
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS, &mut log2phys) };
        if ret == -1 {
            // Not fatal — some file systems don't support this.
            return Ok(None);
        }

        if log2phys.l2p_devoffset >= 0 {
            Ok(Some(log2phys.l2p_devoffset.cast_unsigned()))
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(|e| ScanError::PhysicalOffset {
        path: PathBuf::from("<join-error>"),
        source: std::io::Error::other(e),
    })?
}

/// Get the physical disk offset for a file.
#[cfg(target_os = "linux")]
async fn get_physical_offset(path: &Path) -> Result<Option<u64>, ScanError> {
    use std::os::unix::io::AsRawFd;

    // FIEMAP ioctl constants from `<linux/fs.h>`.
    // libc 0.2 doesn't expose `fiemap` / `fiemap_extent` types or the
    // `FS_IOC_FIEMAP` constant, so we define the struct layout here
    // (matches the Linux kernel ABI since 2.6.28) and use the ioctl number
    // directly via `_IOWR('f', 11, ...)`.
    #[repr(C)]
    #[derive(Default)]
    struct FiemapExtent {
        physical: u64,
        logical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    #[derive(Default)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [FiemapExtent; 0],
    }
    // `_IOWR('f', 11, sizeof(struct fiemap))` on 64-bit Linux.
    const FS_IOC_FIEMAP: libc::c_ulong = 3_223_348_747;

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path).map_err(|e| ScanError::PhysicalOffset {
            path: path.clone(),
            source: e,
        })?;

        // SAFETY: We allocate a buffer that holds a `Fiemap` followed by space
        // for one `FiemapExtent`, zero-initialize it, and pass a valid fd and
        // a properly-aligned pointer to ioctl. The kernel writes up to
        // `extent_count` extents into the trailing region.
        let mut fm: Fiemap = Fiemap {
            start: 0,
            length: u64::MAX,
            flags: 0,
            extent_count: 1,
            ..Default::default()
        };
        let fm_ptr = std::ptr::addr_of_mut!(fm).cast::<libc::c_void>();
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP, fm_ptr) };
        if ret == -1 {
            return Ok(None);
        }

        if fm.mapped_extents > 0 {
            // SAFETY: The kernel has populated up to `extent_count` extents
            // immediately after the `Fiemap` header in our buffer.
            let extent_ptr = unsafe { std::ptr::addr_of!(fm.extents).cast::<FiemapExtent>() };
            let physical = unsafe { (*extent_ptr).physical };
            Ok(Some(physical))
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(|e| ScanError::PhysicalOffset {
        path: PathBuf::from("<join-error>"),
        source: std::io::Error::other(e),
    })?
}

/// Stub for platforms without physical offset resolution.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn get_physical_offset(_path: &Path) -> Result<Option<u64>, ScanError> {
    Ok(None)
}

/// Get the physical disk offset for a file on Windows (NTFS).
#[cfg(target_os = "windows")]
async fn get_physical_offset(path: &Path) -> Result<Option<u64>, ScanError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        super::windows_io::get_ntfs_physical_offset(&path)
            .map_err(|e| ScanError::PhysicalOffset { path, source: e })
    })
    .await
    .map_err(|e| ScanError::PhysicalOffset {
        path: PathBuf::from("<join-error>"),
        source: std::io::Error::other(e),
    })?
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::ports::NullProgress;

    fn runtime() -> Result<tokio::runtime::Runtime, Box<dyn std::error::Error>> {
        Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?)
    }

    #[test]
    fn scan_empty_directory() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let result = scan_tree(tmp.path(), &ScanConfig::default(), &NullProgress).await?;
            assert!(result.entries.is_empty());
            assert!(result.errors.is_empty());
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn scan_known_structure() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let root = tmp.path();

            // Create a known directory structure.
            fs::create_dir_all(root.join("subdir"))?;
            fs::write(root.join("a.txt"), "hello")?;
            fs::write(root.join("b.txt"), "world")?;
            fs::write(root.join("subdir/c.txt"), "nested")?;

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress).await?;

            assert_eq!(result.entries.len(), 3);
            assert!(result.errors.is_empty());

            // Verify all files are present.
            let paths: Vec<_> = result.entries.iter().map(|e| e.path.clone()).collect();
            assert!(paths.contains(&root.join("a.txt")));
            assert!(paths.contains(&root.join("b.txt")));
            assert!(paths.contains(&root.join("subdir/c.txt")));
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn scan_exclude_patterns() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let root = tmp.path();

            fs::write(root.join("keep.txt"), "yes")?;
            fs::write(root.join("skip.log"), "no")?;
            fs::write(root.join("skip.tmp"), "no")?;

            let config = ScanConfig {
                exclude_patterns: vec![Pattern::new("*.log")?, Pattern::new("*.tmp")?],
                follow_symlinks: true,
            };

            let result = scan_tree(root, &config, &NullProgress).await?;

            assert_eq!(result.entries.len(), 1);
            assert!(
                result
                    .entries
                    .first()
                    .is_some_and(|e| e.path.ends_with("keep.txt"))
            );
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn scan_nonexistent_source_returns_error() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let result = scan_tree(
                Path::new("/nonexistent/path/that/does/not/exist"),
                &ScanConfig::default(),
                &NullProgress,
            )
            .await;

            assert!(result.is_err());
            let err = result.err().ok_or("expected error")?;
            if !matches!(err, ScanError::SourceNotFound(_)) {
                return Err(format!("expected SourceNotFound, got {err:?}").into());
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn scan_collects_file_sizes() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let root = tmp.path();

            fs::write(root.join("small.txt"), "hi")?;
            fs::write(root.join("bigger.txt"), "hello world, this is bigger")?;

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress).await?;

            assert_eq!(result.entries.len(), 2);
            // Find each file and check sizes.
            for entry in &result.entries {
                if entry.path.ends_with("small.txt") {
                    assert_eq!(entry.size, 2);
                } else if entry.path.ends_with("bigger.txt") {
                    assert_eq!(entry.size, 27);
                }
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn scan_entries_sorted_by_physical_offset() -> Result<(), Box<dyn std::error::Error>> {
        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let root = tmp.path();

            // Create several files.
            for i in 0..5 {
                fs::write(root.join(format!("file_{i}.txt")), format!("content {i}"))?;
            }

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress).await?;

            assert_eq!(result.entries.len(), 5);

            // Entries should be sorted by physical_offset (or u64::MAX if None).
            for window in result.entries.windows(2) {
                let a = window
                    .first()
                    .map(|e| e.physical_offset.unwrap_or(u64::MAX));
                let b = window.get(1).map(|e| e.physical_offset.unwrap_or(u64::MAX));
                if let (Some(a_off), Some(b_off)) = (a, b) {
                    assert!(a_off <= b_off);
                }
            }
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }

    #[test]
    fn is_excluded_matches_filename() -> Result<(), Box<dyn std::error::Error>> {
        let pattern = Pattern::new("*.log")?;
        let root = Path::new("/root");
        assert!(is_excluded(
            Path::new("/root/test.log"),
            root,
            std::slice::from_ref(&pattern)
        ));
        assert!(!is_excluded(
            Path::new("/root/test.txt"),
            root,
            std::slice::from_ref(&pattern)
        ));
        Ok(())
    }

    #[test]
    fn is_excluded_matches_relative_path() -> Result<(), Box<dyn std::error::Error>> {
        let pattern = Pattern::new("subdir/*.log")?;
        let root = Path::new("/root");
        assert!(is_excluded(
            Path::new("/root/subdir/test.log"),
            root,
            std::slice::from_ref(&pattern)
        ));
        assert!(!is_excluded(
            Path::new("/root/other/test.log"),
            root,
            std::slice::from_ref(&pattern)
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn scan_collects_permissions() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let rt = runtime()?;
        rt.block_on(async {
            let tmp = TempDir::new()?;
            let root = tmp.path();

            let file_path = root.join("perms.txt");
            fs::write(&file_path, "test")?;
            fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755))?;

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress).await?;

            assert_eq!(result.entries.len(), 1);
            let entry = result.entries.first().ok_or("expected one entry")?;
            // Check that permission bits include the executable bit.
            assert_ne!(entry.permissions & 0o111, 0);
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    }
}
