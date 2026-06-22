//! File system port — scanning, metadata, hard-linking.

use std::path::Path;

use crate::domain::{FileEntry, ScanError, SpaceError};

/// Port for file system operations.
pub trait FileSystemPort: Send + Sync {
    /// Recursively scan a directory tree and return all file entries.
    fn scan_tree(
        &self,
        root: &Path,
    ) -> impl Future<Output = Result<Vec<FileEntry>, ScanError>> + Send;

    /// Resolve physical disk offsets for the given entries (for seek-order sorting).
    /// Entries without resolvable offsets keep `physical_offset = None`.
    fn read_physical_offsets(
        &self,
        entries: &mut [FileEntry],
    ) -> impl Future<Output = Result<(), ScanError>> + Send;

    /// Check free space at the given path, returning available bytes.
    fn check_free_space(&self, path: &Path)
    -> impl Future<Output = Result<u64, SpaceError>> + Send;

    /// Create a hard link from `src` to `dst`.
    fn create_hard_link(
        &self,
        src: &Path,
        dst: &Path,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send;

    /// Set POSIX permissions on a file.
    fn set_permissions(
        &self,
        path: &Path,
        mode: u32,
    ) -> impl Future<Output = Result<(), std::io::Error>> + Send;
}
