//! Domain layer — pure data types with no I/O dependencies.

mod error;
mod types;

pub use error::{CopyError, HashError, ScanError, SpaceError, SshError, VerifyError};
pub use types::{CopyPlan, CopyStats, DedupGroup, FileEntry, Hash, HashAlgorithm, TransferMode};
