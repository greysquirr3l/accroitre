//! Verifier port — post-copy integrity checking.

use std::path::Path;

use crate::domain::{HashAlgorithm, VerifyError};

/// Port for post-copy verification.
pub trait VerifierPort: Send + Sync {
    /// Verify a copied file matches its expected hash and size.
    fn verify_file(
        &self,
        path: &Path,
        expected_hash: &str,
        expected_size: u64,
        algorithm: HashAlgorithm,
    ) -> impl Future<Output = Result<(), VerifyError>> + Send;
}
