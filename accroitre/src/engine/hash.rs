//! Parallel file hashing with xxHash-128 and BLAKE3.

use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rayon::prelude::*;
use tracing::warn;

use crate::domain::{FileEntry, Hash, HashAlgorithm, HashError};
use crate::ports::{ProgressPort, ProgressUpdate};

/// Default read buffer size for hashing (64 KiB).
const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

/// Configuration for the hashing engine.
#[derive(Debug, Clone)]
pub struct HashConfig {
    /// Which hash algorithm to use.
    pub algorithm: HashAlgorithm,
    /// Read buffer size in bytes (default: 64 KiB).
    pub buffer_size: usize,
    /// Number of rayon threads (None = rayon default, usually `num_cpus`).
    pub thread_count: Option<usize>,
}

impl Default for HashConfig {
    fn default() -> Self {
        Self {
            algorithm: HashAlgorithm::XxHash128,
            buffer_size: DEFAULT_BUFFER_SIZE,
            thread_count: None,
        }
    }
}

/// Result of a batch hashing operation.
#[derive(Debug)]
pub struct HashResult {
    /// Number of files successfully hashed.
    pub files_hashed: u64,
    /// Total bytes read during hashing.
    pub bytes_hashed: u64,
    /// Number of files that hit the cache (not re-hashed).
    pub cache_hits: u64,
    /// Errors encountered (non-fatal per-file errors).
    pub errors: Vec<HashError>,
}

/// Hash a single file on disk.
///
/// # Errors
///
/// Returns `HashError::Open` if the file cannot be opened, or
/// `HashError::Io` if reading fails.
pub fn hash_file(
    path: &Path,
    algorithm: HashAlgorithm,
    buffer_size: usize,
) -> Result<Hash, HashError> {
    let mut file = File::open(path).map_err(|e| HashError::Open {
        path: path.to_path_buf(),
        source: e,
    })?;

    match algorithm {
        HashAlgorithm::XxHash128 => hash_file_xxhash(&mut file, path, buffer_size),
        HashAlgorithm::Blake3 => hash_file_blake3(&mut file, path, buffer_size),
    }
}

/// Hash raw bytes in memory.
#[must_use]
pub fn hash_bytes(data: &[u8], algorithm: HashAlgorithm) -> Hash {
    match algorithm {
        HashAlgorithm::XxHash128 => {
            let digest = xxhash_rust::xxh3::xxh3_128(data);
            Hash::XxHash128(digest.to_be_bytes())
        }
        HashAlgorithm::Blake3 => {
            let digest = blake3::hash(data);
            Hash::Blake3(*digest.as_bytes())
        }
    }
}

/// Hash all entries in parallel using rayon.
///
/// Updates each `FileEntry.hash` in place. Errors on individual files are
/// collected (non-fatal) and returned in `HashResult.errors`.
///
/// # Errors
///
/// This function does not return a top-level error. Per-file errors are
/// collected in `HashResult.errors`.
pub fn hash_entries(
    entries: &mut [FileEntry],
    config: &HashConfig,
    progress: &dyn ProgressPort,
) -> HashResult {
    let files_total = entries.len() as u64;
    let bytes_total: u64 = entries.iter().map(|e| e.size).sum();
    let bytes_hashed = AtomicU64::new(0);
    let files_hashed = AtomicU64::new(0);

    // Build a custom thread pool if requested.
    let pool = config.thread_count.map(|n| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .ok()
    });

    let results: Vec<(usize, Result<Hash, HashError>)> = if let Some(Some(ref pool)) = pool {
        pool.install(|| {
            hash_entries_parallel(
                entries,
                config,
                &bytes_hashed,
                &files_hashed,
                bytes_total,
                files_total,
                progress,
            )
        })
    } else {
        hash_entries_parallel(
            entries,
            config,
            &bytes_hashed,
            &files_hashed,
            bytes_total,
            files_total,
            progress,
        )
    };

    let mut errors = Vec::new();
    let mut hashed_count = 0u64;

    for (idx, result) in results {
        match result {
            Ok(hash) => {
                if let Some(entry) = entries.get_mut(idx) {
                    entry.hash = Some(hash);
                }
                hashed_count += 1;
            }
            Err(e) => {
                warn!("{e}");
                errors.push(e);
            }
        }
    }

    progress.update(&ProgressUpdate::PhaseComplete { phase: "hash" });

    HashResult {
        files_hashed: hashed_count,
        bytes_hashed: bytes_hashed.load(Ordering::Relaxed),
        cache_hits: 0, // TODO(T12): integrate with CachePort
        errors,
    }
}

fn hash_entries_parallel(
    entries: &[FileEntry],
    config: &HashConfig,
    bytes_hashed: &AtomicU64,
    files_hashed: &AtomicU64,
    bytes_total: u64,
    files_total: u64,
    progress: &dyn ProgressPort,
) -> Vec<(usize, Result<Hash, HashError>)> {
    entries
        .par_iter()
        .enumerate()
        .map(|(idx, entry)| {
            let result = hash_file(&entry.path, config.algorithm, config.buffer_size);

            if result.is_ok() {
                bytes_hashed.fetch_add(entry.size, Ordering::Relaxed);
                let done = files_hashed.fetch_add(1, Ordering::Relaxed) + 1;

                progress.update(&ProgressUpdate::HashProgress {
                    files_hashed: done,
                    files_total,
                    bytes_hashed: bytes_hashed.load(Ordering::Relaxed),
                    bytes_total,
                });
            }

            (idx, result)
        })
        .collect()
}

fn hash_file_xxhash(
    file: &mut File,
    path: &Path,
    buffer_size: usize,
) -> Result<Hash, HashError> {
    let mut hasher = xxhash_rust::xxh3::Xxh3Default::new();
    let mut buf = vec![0u8; buffer_size];

    loop {
        let n = file.read(&mut buf).map_err(|e| HashError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
    }

    let digest = hasher.digest128();
    Ok(Hash::XxHash128(digest.to_be_bytes()))
}

fn hash_file_blake3(
    file: &mut File,
    path: &Path,
    buffer_size: usize,
) -> Result<Hash, HashError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; buffer_size];

    loop {
        let n = file.read(&mut buf).map_err(|e| HashError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
    }

    let digest = hasher.finalize();
    Ok(Hash::Blake3(*digest.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::ports::NullProgress;

    #[test]
    fn hash_bytes_xxhash128_consistency() {
        let data = b"hello world";
        let h1 = hash_bytes(data, HashAlgorithm::XxHash128);
        let h2 = hash_bytes(data, HashAlgorithm::XxHash128);
        assert_eq!(h1, h2);
        assert_eq!(h1.algorithm(), HashAlgorithm::XxHash128);
    }

    #[test]
    fn hash_bytes_blake3_consistency() {
        let data = b"hello world";
        let h1 = hash_bytes(data, HashAlgorithm::Blake3);
        let h2 = hash_bytes(data, HashAlgorithm::Blake3);
        assert_eq!(h1, h2);
        assert_eq!(h1.algorithm(), HashAlgorithm::Blake3);
    }

    #[test]
    fn hash_bytes_different_algorithms_differ() {
        let data = b"hello world";
        let xx = hash_bytes(data, HashAlgorithm::XxHash128);
        let b3 = hash_bytes(data, HashAlgorithm::Blake3);
        assert_ne!(xx, b3);
    }

    #[test]
    fn hash_bytes_blake3_known_digest() {
        // BLAKE3 of "hello world" is well-known.
        let data = b"hello world";
        let h = hash_bytes(data, HashAlgorithm::Blake3);
        let expected = blake3::hash(data);
        if let Hash::Blake3(bytes) = h {
            assert_eq!(&bytes, expected.as_bytes());
        } else {
            panic!("expected Blake3 variant");
        }
    }

    #[test]
    fn hash_bytes_xxhash128_known_digest() {
        let data = b"hello world";
        let h = hash_bytes(data, HashAlgorithm::XxHash128);
        let expected = xxhash_rust::xxh3::xxh3_128(data);
        if let Hash::XxHash128(bytes) = h {
            assert_eq!(bytes, expected.to_be_bytes());
        } else {
            panic!("expected XxHash128 variant");
        }
    }

    #[test]
    fn hash_file_xxhash128() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("test.bin");
        fs::write(&path, b"file content for hashing").expect("write");

        let result = hash_file(&path, HashAlgorithm::XxHash128, DEFAULT_BUFFER_SIZE);
        assert!(result.is_ok());
        let hash = result.expect("hash");
        assert_eq!(hash.algorithm(), HashAlgorithm::XxHash128);

        // Same file should produce same hash.
        let result2 = hash_file(&path, HashAlgorithm::XxHash128, DEFAULT_BUFFER_SIZE);
        assert_eq!(hash, result2.expect("hash2"));
    }

    #[test]
    fn hash_file_blake3() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("test.bin");
        fs::write(&path, b"file content for hashing").expect("write");

        let result = hash_file(&path, HashAlgorithm::Blake3, DEFAULT_BUFFER_SIZE);
        assert!(result.is_ok());
        let hash = result.expect("hash");
        assert_eq!(hash.algorithm(), HashAlgorithm::Blake3);
    }

    #[test]
    fn hash_file_nonexistent_returns_error() {
        let result = hash_file(
            Path::new("/nonexistent/file.txt"),
            HashAlgorithm::XxHash128,
            DEFAULT_BUFFER_SIZE,
        );
        assert!(result.is_err());
    }

    #[test]
    fn hash_entries_parallel_all_files() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        // Create test files.
        for i in 0..10 {
            fs::write(root.join(format!("file_{i}.txt")), format!("content {i}"))
                .expect("write");
        }

        let mut entries: Vec<FileEntry> = (0..10)
            .map(|i| {
                let path = root.join(format!("file_{i}.txt"));
                let size = fs::metadata(&path).expect("metadata").len();
                FileEntry::new(path, size)
            })
            .collect();

        let config = HashConfig::default();
        let result = hash_entries(&mut entries, &config, &NullProgress);

        assert_eq!(result.files_hashed, 10);
        assert!(result.errors.is_empty());

        // All entries should now have hashes.
        for entry in &entries {
            assert!(entry.hash.is_some());
        }
    }

    #[test]
    fn hash_entries_identical_files_same_hash() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        let content = "identical content";
        fs::write(root.join("a.txt"), content).expect("write");
        fs::write(root.join("b.txt"), content).expect("write");

        let mut entries = vec![
            FileEntry::new(
                root.join("a.txt"),
                content.len() as u64,
            ),
            FileEntry::new(
                root.join("b.txt"),
                content.len() as u64,
            ),
        ];

        let config = HashConfig::default();
        hash_entries(&mut entries, &config, &NullProgress);

        let hash_a = entries.first().and_then(|e| e.hash.as_ref());
        let hash_b = entries.get(1).and_then(|e| e.hash.as_ref());
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn hash_entries_different_files_different_hash() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        fs::write(root.join("a.txt"), "content a").expect("write");
        fs::write(root.join("b.txt"), "content b").expect("write");

        let mut entries = vec![
            FileEntry::new(root.join("a.txt"), 9),
            FileEntry::new(root.join("b.txt"), 9),
        ];

        let config = HashConfig::default();
        hash_entries(&mut entries, &config, &NullProgress);

        let hash_a = entries.first().and_then(|e| e.hash.as_ref());
        let hash_b = entries.get(1).and_then(|e| e.hash.as_ref());
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn hash_entries_custom_thread_count() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("test.txt");
        fs::write(&path, "test data").expect("write");

        let mut entries = vec![FileEntry::new(path, 9)];
        let config = HashConfig {
            thread_count: Some(1),
            ..HashConfig::default()
        };

        let result = hash_entries(&mut entries, &config, &NullProgress);
        assert_eq!(result.files_hashed, 1);
    }

    #[test]
    fn hash_empty_file() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("empty.txt");
        fs::write(&path, b"").expect("write");

        let result = hash_file(&path, HashAlgorithm::XxHash128, DEFAULT_BUFFER_SIZE);
        assert!(result.is_ok());

        let result_b3 = hash_file(&path, HashAlgorithm::Blake3, DEFAULT_BUFFER_SIZE);
        assert!(result_b3.is_ok());
    }
}
