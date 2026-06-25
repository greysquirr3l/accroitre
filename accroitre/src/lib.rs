//! Accroître — high-speed, cross-platform file copier with deduplication and SSH streaming.
//!
//! This crate provides the library core: domain types, port traits, engine logic,
//! and adapter implementations. The binary CLI lives in the `accroitre-cli` crate.
//!
//! # Quick start
//!
//! ```no_run
//! use accroitre::{scan_tree, dedup_with_hashing, execute_copy_plan, verify_plan,
//!                 CopyConfig, ScanConfig, VerifyConfig, HashAlgorithm, NullProgress};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let progress = NullProgress;
//! let scan = scan_tree(std::path::Path::new("/src"), &ScanConfig::default(), &progress).await?;
//! let (plan, _stats) = dedup_with_hashing(scan.entries, "/src".as_ref(), "/dst".as_ref(),
//!                                          HashAlgorithm::XxHash128);
//! execute_copy_plan(&plan, &CopyConfig::default(), &progress)?;
//! let verify = verify_plan(&plan, &VerifyConfig::default(), &progress);
//! assert!(verify.failures.is_empty());
//! # Ok(())
//! # }
//! ```
//!
//! See the `examples/` directory for runnable end-to-end programs.

// Module hierarchy: domain is pure, ports are traits, engine orchestrates,
// adapters implement the port traits.
pub mod adapters;
pub mod domain;
pub mod engine;
pub mod ports;

// Convenience re-exports: the most-used items live at the crate root so
// callers don't need to know the internal module layout.
//
// We deliberately re-export `execute_copy_plan`, `dedup_with_hashing`,
// `verify_plan`, and `scan_tree` so consumers can do
// `use accroitre::scan_tree;` rather than the longer
// `use accroitre::engine::scan::scan_tree;`.

// Domain types
pub use domain::{
    CopyError, CopyPlan, CopyStats, DedupGroup, FileEntry, Hash, HashAlgorithm, SpaceError,
    SshError, TransferMode, VerifyError,
};

// Engine entry points
pub use engine::copy::{
    CopyConfig, CopyResult, LinkStrategy, execute_copy_plan, execute_copy_plan_resumable,
};
pub use engine::dedup::{DedupStats, build_dedup_plan, dedup_with_hashing};
pub use engine::delta::{DeltaResult, compute_delta, delete_orphans, find_orphans};
pub use engine::hash::{HashConfig, HashResult, hash_bytes, hash_entries, hash_file};
pub use engine::scan::{ScanConfig, ScanResult, scan_tree};
pub use engine::verify::{VerifyConfig, VerifyResult, verify_plan};

// Adapter entry points (for users embedding accroitre)
pub use adapters::cache::{CacheError, SqliteCache};
pub use adapters::lock::{DestLock, LockError};
pub use adapters::manifest::{CopyManifest, FileStatus, ManifestEntry};
pub use adapters::ssh::{AuthMethod, SshAdapter, SshConfig};

// Port traits and their default no-op implementation
pub use ports::{NullProgress, ProgressPort, ProgressUpdate};
