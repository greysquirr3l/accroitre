//! Copier port — file copy and tar-streaming operations.

use std::path::Path;

use crate::domain::{CopyError, FileEntry};

/// Port for file copy operations.
pub trait CopierPort: Send + Sync {
    /// Copy a single file with large-buffer I/O.
    fn copy_file(
        &self,
        src: &Path,
        dst: &Path,
    ) -> impl Future<Output = Result<u64, CopyError>> + Send;

    /// Stream a batch of small files as a tar archive from source to destination.
    fn stream_batch(
        &self,
        entries: &[FileEntry],
        source_root: &Path,
        dest_root: &Path,
    ) -> impl Future<Output = Result<u64, CopyError>> + Send;
}
