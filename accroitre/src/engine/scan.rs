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
                let mut entry = FileEntry::new(path, metadata.len());
                entry.permissions = permissions;
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
                debug!("could not get physical offset for {}: {e}", entry.path.display());
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

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path).map_err(|e| ScanError::PhysicalOffset {
            path: path.clone(),
            source: e,
        })?;

        // Use FIEMAP ioctl to get the physical extent offset of the first extent.
        // SAFETY: We allocate a properly sized buffer for the fiemap struct + 1 extent,
        // zero-initialize it, and pass valid fd/pointer to ioctl.
        let mut buf = [0u8; std::mem::size_of::<libc::fiemap>() + std::mem::size_of::<libc::fiemap_extent>()];
        let fiemap = buf.as_mut_ptr().cast::<libc::fiemap>();
        unsafe {
            (*fiemap).fm_start = 0;
            (*fiemap).fm_length = u64::MAX;
            (*fiemap).fm_flags = 0;
            (*fiemap).fm_extent_count = 1;
        }

        let ret = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_FIEMAP, fiemap) };
        if ret == -1 {
            return Ok(None);
        }

        let mapped_extents = unsafe { (*fiemap).fm_mapped_extents };
        if mapped_extents > 0 {
            let extents_ptr = unsafe { fiemap.add(1).cast::<libc::fiemap_extent>() };
            let physical = unsafe { (*extents_ptr).fe_physical };
            Ok(Some(physical))
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(|e| ScanError::PhysicalOffset {
        path: PathBuf::from("<join-error>"),
        source: std::io::Error::new(std::io::ErrorKind::Other, e),
    })?
}

/// Stub for platforms without physical offset resolution.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn get_physical_offset(_path: &Path) -> Result<Option<u64>, ScanError> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::ports::NullProgress;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn scan_empty_directory() {
        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let result = scan_tree(tmp.path(), &ScanConfig::default(), &NullProgress).await;
            let result = result.expect("scan should succeed");
            assert!(result.entries.is_empty());
            assert!(result.errors.is_empty());
        });
    }

    #[test]
    fn scan_known_structure() {
        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path();

            // Create a known directory structure.
            fs::create_dir_all(root.join("subdir")).expect("mkdir");
            fs::write(root.join("a.txt"), "hello").expect("write");
            fs::write(root.join("b.txt"), "world").expect("write");
            fs::write(root.join("subdir/c.txt"), "nested").expect("write");

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress)
                .await
                .expect("scan should succeed");

            assert_eq!(result.entries.len(), 3);
            assert!(result.errors.is_empty());

            // Verify all files are present.
            let paths: Vec<_> = result.entries.iter().map(|e| e.path.clone()).collect();
            assert!(paths.contains(&root.join("a.txt")));
            assert!(paths.contains(&root.join("b.txt")));
            assert!(paths.contains(&root.join("subdir/c.txt")));
        });
    }

    #[test]
    fn scan_exclude_patterns() {
        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path();

            fs::write(root.join("keep.txt"), "yes").expect("write");
            fs::write(root.join("skip.log"), "no").expect("write");
            fs::write(root.join("skip.tmp"), "no").expect("write");

            let config = ScanConfig {
                exclude_patterns: vec![
                    Pattern::new("*.log").expect("pattern"),
                    Pattern::new("*.tmp").expect("pattern"),
                ],
                follow_symlinks: true,
            };

            let result = scan_tree(root, &config, &NullProgress)
                .await
                .expect("scan should succeed");

            assert_eq!(result.entries.len(), 1);
            assert!(result.entries.first().is_some_and(|e| e.path.ends_with("keep.txt")));
        });
    }

    #[test]
    fn scan_nonexistent_source_returns_error() {
        let rt = runtime();
        rt.block_on(async {
            let result = scan_tree(
                Path::new("/nonexistent/path/that/does/not/exist"),
                &ScanConfig::default(),
                &NullProgress,
            )
            .await;

            assert!(result.is_err());
            let err = result.unwrap_err();
            matches!(err, ScanError::SourceNotFound(_));
        });
    }

    #[test]
    fn scan_collects_file_sizes() {
        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path();

            fs::write(root.join("small.txt"), "hi").expect("write");
            fs::write(root.join("bigger.txt"), "hello world, this is bigger").expect("write");

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress)
                .await
                .expect("scan should succeed");

            assert_eq!(result.entries.len(), 2);
            // Find each file and check sizes.
            for entry in &result.entries {
                if entry.path.ends_with("small.txt") {
                    assert_eq!(entry.size, 2);
                } else if entry.path.ends_with("bigger.txt") {
                    assert_eq!(entry.size, 27);
                }
            }
        });
    }

    #[test]
    fn scan_entries_sorted_by_physical_offset() {
        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path();

            // Create several files.
            for i in 0..5 {
                fs::write(root.join(format!("file_{i}.txt")), format!("content {i}"))
                    .expect("write");
            }

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress)
                .await
                .expect("scan should succeed");

            assert_eq!(result.entries.len(), 5);

            // Entries should be sorted by physical_offset (or u64::MAX if None).
            for window in result.entries.windows(2) {
                let a = window.first().map(|e| e.physical_offset.unwrap_or(u64::MAX));
                let b = window.get(1).map(|e| e.physical_offset.unwrap_or(u64::MAX));
                if let (Some(a_off), Some(b_off)) = (a, b) {
                    assert!(a_off <= b_off);
                }
            }
        });
    }

    #[test]
    fn is_excluded_matches_filename() {
        let pattern = Pattern::new("*.log").expect("pattern");
        let root = Path::new("/root");
        assert!(is_excluded(Path::new("/root/test.log"), root, &[pattern.clone()]));
        assert!(!is_excluded(Path::new("/root/test.txt"), root, &[pattern]));
    }

    #[test]
    fn is_excluded_matches_relative_path() {
        let pattern = Pattern::new("subdir/*.log").expect("pattern");
        let root = Path::new("/root");
        assert!(is_excluded(
            Path::new("/root/subdir/test.log"),
            root,
            &[pattern.clone()]
        ));
        assert!(!is_excluded(
            Path::new("/root/other/test.log"),
            root,
            &[pattern]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn scan_collects_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let rt = runtime();
        rt.block_on(async {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path();

            let file_path = root.join("perms.txt");
            fs::write(&file_path, "test").expect("write");
            fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755)).expect("chmod");

            let result = scan_tree(root, &ScanConfig::default(), &NullProgress)
                .await
                .expect("scan should succeed");

            assert_eq!(result.entries.len(), 1);
            let entry = result.entries.first().expect("one entry");
            // Check that permission bits include the executable bit.
            assert_ne!(entry.permissions & 0o111, 0);
        });
    }
}
