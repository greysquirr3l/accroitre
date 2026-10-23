//! Tar streaming domain types — batched archive transfer over SSH.
//!
//! These live in the domain layer so that the [`SshPort`](crate::ports::SshPort)
//! trait can describe tar transfers without depending on the SSH adapter.

use crate::domain::CopyError;

/// Default batch size target for chunked tar streaming (~100 MB).
pub const DEFAULT_BATCH_SIZE: u64 = 100 * 1024 * 1024;

/// Result of a tar streaming operation.
#[derive(Debug, Default)]
pub struct TarStreamResult {
    /// Total bytes transferred.
    pub bytes_transferred: u64,
    /// Number of files transferred.
    pub files_transferred: u64,
    /// Number of batches.
    pub batches_completed: u64,
    /// Errors encountered (non-fatal, per-batch).
    pub errors: Vec<CopyError>,
}

impl TarStreamResult {
    /// Whether every batch completed without error.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Configuration for tar streaming over SSH.
#[derive(Debug, Clone)]
pub struct TarStreamConfig {
    /// Target batch size in bytes (default: ~100 MB).
    pub batch_size: u64,
    /// Enable SSH compression (equivalent to `ssh -C`).
    pub compress: bool,
}

impl Default for TarStreamConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            compress: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_100mb_batches_without_compression() {
        let config = TarStreamConfig::default();
        assert_eq!(config.batch_size, 100 * 1024 * 1024);
        assert!(!config.compress);
    }

    #[test]
    fn empty_result_is_complete() {
        let result = TarStreamResult::default();
        assert!(result.is_complete());
        assert_eq!(result.bytes_transferred, 0);
        assert_eq!(result.files_transferred, 0);
        assert_eq!(result.batches_completed, 0);
    }

    #[test]
    fn result_with_errors_is_not_complete() {
        let result = TarStreamResult {
            errors: vec![CopyError::InsufficientSpace {
                needed: 10,
                available: 1,
            }],
            ..TarStreamResult::default()
        };
        assert!(!result.is_complete());
    }
}
