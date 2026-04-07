//! Cache port — persistent hash cache for cross-run deduplication.

use std::path::Path;

use crate::domain::Hash;

/// Port for the persistent hash cache (e.g. `SQLite`).
pub trait CachePort: Send + Sync {
    /// Error type for cache operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Look up a cached hash by file path and modification time.
    fn lookup_hash(
        &self,
        path: &Path,
        modified_epoch: u64,
        size: u64,
    ) -> impl Future<Output = Result<Option<Hash>, Self::Error>> + Send;

    /// Store a hash in the cache, keyed by path + mtime + size.
    fn store_hash(
        &self,
        path: &Path,
        modified_epoch: u64,
        size: u64,
        hash: &Hash,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Load the full hash cache from persistent storage.
    fn load(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Flush any pending writes to persistent storage.
    fn save(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
