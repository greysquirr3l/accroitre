//! Progress port — UI updates for scan, hash, copy, and verify phases.

use std::path::Path;

/// A progress update event.
#[derive(Debug, Clone)]
pub enum ProgressUpdate<'a> {
    /// Scanning phase: found files so far.
    ScanProgress {
        files_found: u64,
        current_dir: &'a Path,
    },
    /// Hashing phase: files hashed so far out of total.
    HashProgress {
        files_hashed: u64,
        files_total: u64,
        bytes_hashed: u64,
        bytes_total: u64,
    },
    /// Copy phase: files/bytes copied so far.
    CopyProgress {
        files_copied: u64,
        files_total: u64,
        bytes_copied: u64,
        bytes_total: u64,
    },
    /// Verify phase: files verified so far.
    VerifyProgress {
        files_verified: u64,
        files_total: u64,
    },
    /// A phase has completed.
    PhaseComplete {
        phase: &'a str,
    },
    /// An error occurred on a specific file (non-fatal).
    FileError {
        path: &'a Path,
        message: &'a str,
    },
}

/// Port for reporting progress to the UI layer.
///
/// This trait is synchronous — progress updates are fire-and-forget
/// from the engine's perspective. The implementor decides whether to
/// render a TUI, write JSON, or ignore the updates entirely.
pub trait ProgressPort: Send + Sync {
    /// Report a progress update.
    fn update(&self, event: &ProgressUpdate<'_>);

    /// Signal that all operations are complete.
    fn finish(&self);
}

/// A no-op progress port for non-interactive or test usage.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullProgress;

impl ProgressPort for NullProgress {
    fn update(&self, _event: &ProgressUpdate<'_>) {}
    fn finish(&self) {}
}
