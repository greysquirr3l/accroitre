//! Hasher port — content hashing for deduplication and verification.

use std::path::Path;

use crate::domain::{Hash, HashAlgorithm, HashError};

/// Port for file hashing operations.
pub trait HasherPort: Send + Sync {
    /// Hash a file on disk, returning the content hash.
    fn hash_file(
        &self,
        path: &Path,
        algorithm: HashAlgorithm,
    ) -> impl Future<Output = Result<Hash, HashError>> + Send;

    /// Hash raw bytes in memory, returning the content hash.
    fn hash_bytes(&self, data: &[u8], algorithm: HashAlgorithm) -> Hash;
}
