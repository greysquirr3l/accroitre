//! `SQLite` hash cache adapter — persists file hashes across runs.
//!
//! Uses rusqlite (synchronous) since cache I/O is fast enough to not need
//! async. The database lives at `.accroitre-cache.db` in the destination root.
//! WAL mode is enabled for concurrent reads during parallel hashing.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{debug, info};

use crate::domain::Hash;

/// SQLite-backed hash cache.
pub struct SqliteCache {
    /// Database connection, guarded for Send + Sync.
    conn: Mutex<Connection>,
    /// Whether cache operations are enabled.
    enabled: bool,
}

impl SqliteCache {
    /// Open (or create) the cache database at `dest_root/.accroitre-cache.db`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open(dest_root: &Path) -> Result<Self, CacheError> {
        let db_path = dest_root.join(".accroitre-cache.db");
        Self::open_at(&db_path)
    }

    /// Open (or create) the cache database at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open_at(db_path: &Path) -> Result<Self, CacheError> {
        let conn = Connection::open(db_path).map_err(|e| CacheError::Open {
            path: db_path.to_path_buf(),
            source: e,
        })?;

        // Enable WAL mode for concurrent reads.
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| CacheError::Schema { source: e })?;

        // Create schema if it doesn't exist.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path  TEXT    NOT NULL,
                size  INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                hash  BLOB    NOT NULL,
                algo  TEXT    NOT NULL,
                PRIMARY KEY (path)
            );",
        )
        .map_err(|e| CacheError::Schema { source: e })?;

        info!("cache opened at {}", db_path.display());

        Ok(Self {
            conn: Mutex::new(conn),
            enabled: true,
        })
    }

    /// Create a disabled (no-op) cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the in-memory `SQLite` connection cannot be opened.
    pub fn disabled() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory().map_err(|e| CacheError::Schema { source: e })?;
        Ok(Self {
            conn: Mutex::new(conn),
            enabled: false,
        })
    }

    /// Look up a cached hash. Returns `None` if the entry is missing or stale
    /// (size or mtime don't match).
    ///
    /// # Errors
    ///
    /// Returns an error on database query failures.
    pub fn lookup(
        &self,
        path: &Path,
        modified_epoch: u64,
        size: u64,
    ) -> Result<Option<Hash>, CacheError> {
        if !self.enabled {
            return Ok(None);
        }

        let conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::Lock)?;

        let path_str = path.to_string_lossy();

        let result: Option<(Vec<u8>, String, i64, i64)> = conn
            .query_row(
                "SELECT hash, algo, size, mtime FROM files WHERE path = ?1",
                params![path_str.as_ref()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| CacheError::Query { source: e })?;

        let Some((hash_bytes, algo, cached_size, cached_mtime)) = result else {
            return Ok(None);
        };

        // Invalidate if size or mtime changed.
        if cached_size.cast_unsigned() != size || cached_mtime.cast_unsigned() != modified_epoch {
            debug!(
                "cache stale for {}: size {}→{}, mtime {}→{}",
                path.display(),
                cached_size,
                size,
                cached_mtime,
                modified_epoch,
            );
            return Ok(None);
        }

        Ok(decode_hash(&hash_bytes, &algo))
    }

    /// Store a hash in the cache. Uses INSERT OR REPLACE for upsert.
    ///
    /// # Errors
    ///
    /// Returns an error on database write failures.
    pub fn store(
        &self,
        path: &Path,
        modified_epoch: u64,
        size: u64,
        hash: &Hash,
    ) -> Result<(), CacheError> {
        if !self.enabled {
            return Ok(());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::Lock)?;

        let path_str = path.to_string_lossy();
        let (hash_bytes, algo) = encode_hash(hash);

        conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime, hash, algo)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                path_str.as_ref(),
                size.cast_signed(),
                modified_epoch.cast_signed(),
                hash_bytes,
                algo,
            ],
        )
        .map_err(|e| CacheError::Write { source: e })?;

        Ok(())
    }

    /// Batch-store multiple hashes in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error on database write failures.
    pub fn store_batch(
        &self,
        entries: &[(PathBuf, u64, u64, Hash)],
    ) -> Result<(), CacheError> {
        if !self.enabled || entries.is_empty() {
            return Ok(());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::Lock)?;

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| CacheError::Write { source: e })?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO files (path, size, mtime, hash, algo)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| CacheError::Write { source: e })?;

            for (path, modified_epoch, size, hash) in entries {
                let path_str = path.to_string_lossy();
                let (hash_bytes, algo) = encode_hash(hash);

                stmt.execute(params![
                    path_str.as_ref(),
                    (*size).cast_signed(),
                    (*modified_epoch).cast_signed(),
                    hash_bytes,
                    algo,
                ])
                .map_err(|e| CacheError::Write { source: e })?;
            }
        }

        tx.commit()
            .map_err(|e| CacheError::Write { source: e })?;

        debug!("batch stored {} entries", entries.len());

        Ok(())
    }
}

/// Encode a `Hash` to `(bytes, algorithm_name)` for storage.
fn encode_hash(hash: &Hash) -> (Vec<u8>, &'static str) {
    match hash {
        Hash::XxHash128(bytes) => (bytes.to_vec(), "xxhash128"),
        Hash::Blake3(bytes) => (bytes.to_vec(), "blake3"),
    }
}

/// Decode stored bytes + algorithm name back to a `Hash`.
fn decode_hash(bytes: &[u8], algo: &str) -> Option<Hash> {
    match algo {
        "xxhash128" => {
            if bytes.len() == 16 {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(bytes);
                Some(Hash::XxHash128(arr))
            } else {
                None
            }
        }
        "blake3" => {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(bytes);
                Some(Hash::Blake3(arr))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Errors from the `SQLite` cache adapter.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Failed to open the database.
    #[error("failed to open cache database at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// Failed to create or migrate schema.
    #[error("cache schema error")]
    Schema {
        #[source]
        source: rusqlite::Error,
    },
    /// Failed to query the cache.
    #[error("cache query error")]
    Query {
        #[source]
        source: rusqlite::Error,
    },
    /// Failed to write to the cache.
    #[error("cache write error")]
    Write {
        #[source]
        source: rusqlite::Error,
    },
    /// Mutex poisoned.
    #[error("cache lock poisoned")]
    Lock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_xxhash(val: u128) -> Hash {
        Hash::XxHash128(val.to_be_bytes())
    }

    fn make_blake3(seed: u8) -> Hash {
        Hash::Blake3([seed; 32])
    }

    #[test]
    fn store_then_lookup_round_trip() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let path = Path::new("/test/file.txt");
        let hash = make_xxhash(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let mtime = 1_700_000_000;
        let size = 42;

        cache.store(path, mtime, size, &hash).unwrap();
        let result = cache.lookup(path, mtime, size).unwrap();
        assert_eq!(result, Some(hash));
    }

    #[test]
    fn cache_invalidation_on_size_change() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let path = Path::new("/test/file.txt");
        let hash = make_xxhash(0xAAAA_BBBB_CCCC_DDDD_EEEE_FFFF_0000_1111);
        let mtime = 1_700_000_000;
        let original_size = 100;
        let new_size = 200;

        cache.store(path, mtime, original_size, &hash).unwrap();

        // Lookup with different size should return None (stale).
        let result = cache.lookup(path, mtime, new_size).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cache_invalidation_on_mtime_change() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let path = Path::new("/test/file.txt");
        let hash = make_blake3(0x42);
        let original_mtime = 1_700_000_000;
        let new_mtime = 1_700_001_000;
        let size = 100;

        cache.store(path, original_mtime, size, &hash).unwrap();

        // Lookup with different mtime should return None (stale).
        let result = cache.lookup(path, new_mtime, size).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn disabled_cache_returns_none() {
        let cache = SqliteCache::disabled().unwrap();

        let path = Path::new("/test/file.txt");
        let hash = make_xxhash(0x1111_2222_3333_4444_5555_6666_7777_8888);

        // Store should succeed (no-op).
        cache.store(path, 1_000_000, 50, &hash).unwrap();

        // Lookup should return None.
        let result = cache.lookup(path, 1_000_000, 50).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn batch_store_multiple_entries() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let entries = vec![
            (PathBuf::from("/a.txt"), 1_000u64, 10u64, make_xxhash(1)),
            (PathBuf::from("/b.txt"), 2_000, 20, make_blake3(2)),
            (PathBuf::from("/c.txt"), 3_000, 30, make_xxhash(3)),
        ];

        cache.store_batch(&entries).unwrap();

        assert_eq!(
            cache.lookup(Path::new("/a.txt"), 1_000, 10).unwrap(),
            Some(make_xxhash(1))
        );
        assert_eq!(
            cache.lookup(Path::new("/b.txt"), 2_000, 20).unwrap(),
            Some(make_blake3(2))
        );
        assert_eq!(
            cache.lookup(Path::new("/c.txt"), 3_000, 30).unwrap(),
            Some(make_xxhash(3))
        );
    }

    #[test]
    fn store_overwrites_stale_entry() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let path = Path::new("/test/file.txt");
        let hash_v1 = make_xxhash(1);
        let hash_v2 = make_xxhash(2);

        cache.store(path, 1000, 100, &hash_v1).unwrap();
        cache.store(path, 2000, 200, &hash_v2).unwrap();

        // Old entry should be gone.
        assert!(cache.lookup(path, 1000, 100).unwrap().is_none());
        // New entry should be present.
        assert_eq!(cache.lookup(path, 2000, 200).unwrap(), Some(hash_v2));
    }

    #[test]
    fn blake3_round_trip() {
        let dir = TempDir::new().unwrap();
        let cache = SqliteCache::open(dir.path()).unwrap();

        let path = Path::new("/blake3.dat");
        let hash = make_blake3(0xFF);

        cache.store(path, 5000, 512, &hash).unwrap();
        let result = cache.lookup(path, 5000, 512).unwrap();
        assert_eq!(result, Some(hash));
    }
}
