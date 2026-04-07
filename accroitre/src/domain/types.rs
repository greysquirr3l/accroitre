//! Core domain types — pure data structures with no I/O dependencies.

use std::{
    fmt,
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Supported hash algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashAlgorithm {
    XxHash128,
    Blake3,
}

impl fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::XxHash128 => f.write_str("xxHash-128"),
            Self::Blake3 => f.write_str("BLAKE3"),
        }
    }
}

/// A content hash — either xxHash-128 (16 bytes) or BLAKE3 (32 bytes).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Hash {
    XxHash128([u8; 16]),
    Blake3([u8; 32]),
}

impl Hash {
    /// Returns the algorithm used to produce this hash.
    #[must_use]
    pub fn algorithm(&self) -> HashAlgorithm {
        match self {
            Self::XxHash128(_) => HashAlgorithm::XxHash128,
            Self::Blake3(_) => HashAlgorithm::Blake3,
        }
    }

    /// Returns the raw hash bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::XxHash128(b) => b,
            Self::Blake3(b) => b,
        }
    }

    /// Returns the hex-encoded string for this hash.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.as_bytes()
            .iter()
            .fold(String::new(), |mut acc, byte| {
                // write_fmt to a String is infallible — it only fails on
                // Formatter width/precision issues which don't apply here.
                let _ = fmt::Write::write_fmt(&mut acc, format_args!("{byte:02x}"));
                acc
            })
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::XxHash128(_) => write!(f, "XxHash128({})", self.to_hex()),
            Self::Blake3(_) => write!(f, "Blake3({})", self.to_hex()),
        }
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Metadata for a single file discovered during scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Absolute or relative path to the file.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Content hash, populated after the hashing phase.
    pub hash: Option<Hash>,
    /// Physical disk offset for seek-order sorting (platform-dependent).
    pub physical_offset: Option<u64>,
    /// POSIX permission bits (0 on platforms without POSIX permissions).
    pub permissions: u32,
}

impl FileEntry {
    /// Create a new `FileEntry` with only path and size known.
    #[must_use]
    pub fn new(path: PathBuf, size: u64) -> Self {
        Self {
            path,
            size,
            hash: None,
            physical_offset: None,
            permissions: 0,
        }
    }
}

/// A group of files sharing the same content hash.
///
/// The `canonical` index points to the file that will be physically copied;
/// `duplicates` lists indices of files that will be hard-linked to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupGroup {
    /// Index into the `CopyPlan::entries` vec for the canonical (copied) file.
    pub canonical: usize,
    /// Indices into `CopyPlan::entries` for duplicate files (hard-linked).
    pub duplicates: Vec<usize>,
}

/// A fully resolved plan describing what to copy and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyPlan {
    /// Root directory being copied from.
    pub source_root: PathBuf,
    /// Root directory being copied to.
    pub dest_root: PathBuf,
    /// Files to copy, sorted by `physical_offset` for sequential I/O.
    pub entries: Vec<FileEntry>,
    /// Groups of files sharing identical content.
    pub dedup_groups: Vec<DedupGroup>,
}

impl CopyPlan {
    /// Create a new empty copy plan.
    #[must_use]
    pub fn new(source_root: PathBuf, dest_root: PathBuf) -> Self {
        Self {
            source_root,
            dest_root,
            entries: Vec::new(),
            dedup_groups: Vec::new(),
        }
    }

    /// Total bytes that need to be transferred (before dedup savings).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Number of files that will be hard-linked instead of copied.
    #[must_use]
    pub fn link_count(&self) -> usize {
        self.dedup_groups.iter().map(|g| g.duplicates.len()).sum()
    }
}

/// How files are being transferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransferMode {
    LocalToLocal,
    LocalToRemote,
    RemoteToLocal,
    RemoteToRemote,
}

impl fmt::Display for TransferMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalToLocal => f.write_str("local → local"),
            Self::LocalToRemote => f.write_str("local → remote"),
            Self::RemoteToLocal => f.write_str("remote → local"),
            Self::RemoteToRemote => f.write_str("remote → remote"),
        }
    }
}

/// Aggregate statistics from a completed (or in-progress) copy operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CopyStats {
    pub files_total: u64,
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_skipped: u64,
    pub files_errored: u64,
    pub bytes_written: u64,
    pub bytes_saved_dedup: u64,
    pub elapsed: Duration,
}

impl CopyStats {
    /// Throughput in bytes per second, or `None` if elapsed is zero.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // acceptable: throughput display doesn't need u64 precision
    pub fn throughput_bps(&self) -> Option<f64> {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            Some(self.bytes_written as f64 / secs)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_xxhash128_display_and_eq() {
        let h1 = Hash::XxHash128([0xAA; 16]);
        let h2 = Hash::XxHash128([0xAA; 16]);
        let h3 = Hash::XxHash128([0xBB; 16]);

        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        // 16 bytes * 2 hex chars/byte = 32 hex chars
        assert_eq!(h1.to_hex().len(), 32);
        assert_eq!(h1.to_string(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn hash_blake3_display_and_eq() {
        let h = Hash::Blake3([0x01; 32]);
        assert_eq!(h.to_hex().len(), 64);
        assert_eq!(
            h.to_string(),
            "0101010101010101010101010101010101010101010101010101010101010101"
        );
    }

    #[test]
    fn hash_algorithm() {
        let xx = Hash::XxHash128([0; 16]);
        let b3 = Hash::Blake3([0; 32]);
        assert_eq!(xx.algorithm(), HashAlgorithm::XxHash128);
        assert_eq!(b3.algorithm(), HashAlgorithm::Blake3);
    }

    #[test]
    fn hash_algorithm_display() {
        assert_eq!(HashAlgorithm::XxHash128.to_string(), "xxHash-128");
        assert_eq!(HashAlgorithm::Blake3.to_string(), "BLAKE3");
    }

    #[test]
    fn hash_cross_algorithm_not_equal() {
        let xx = Hash::XxHash128([0; 16]);
        let b3 = Hash::Blake3([0; 32]);
        assert_ne!(xx, b3);
    }

    #[test]
    fn file_entry_new_defaults() {
        let entry = FileEntry::new(PathBuf::from("/test.txt"), 1024);
        assert_eq!(entry.path, PathBuf::from("/test.txt"));
        assert_eq!(entry.size, 1024);
        assert!(entry.hash.is_none());
        assert!(entry.physical_offset.is_none());
        assert_eq!(entry.permissions, 0);
    }

    #[test]
    fn copy_plan_construction_and_stats() {
        let mut plan = CopyPlan::new(PathBuf::from("/src"), PathBuf::from("/dst"));

        plan.entries.push(FileEntry {
            path: PathBuf::from("/src/a.txt"),
            size: 100,
            hash: Some(Hash::XxHash128([0xAA; 16])),
            physical_offset: Some(0),
            permissions: 0o644,
        });
        plan.entries.push(FileEntry {
            path: PathBuf::from("/src/b.txt"),
            size: 100,
            hash: Some(Hash::XxHash128([0xAA; 16])),
            physical_offset: Some(1024),
            permissions: 0o644,
        });
        plan.entries.push(FileEntry {
            path: PathBuf::from("/src/c.txt"),
            size: 200,
            hash: Some(Hash::XxHash128([0xBB; 16])),
            physical_offset: Some(2048),
            permissions: 0o755,
        });

        plan.dedup_groups.push(DedupGroup {
            canonical: 0,
            duplicates: vec![1],
        });

        assert_eq!(plan.total_bytes(), 400);
        assert_eq!(plan.link_count(), 1);
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.dedup_groups.len(), 1);
    }

    #[test]
    fn transfer_mode_display() {
        assert_eq!(TransferMode::LocalToLocal.to_string(), "local → local");
        assert_eq!(TransferMode::LocalToRemote.to_string(), "local → remote");
        assert_eq!(TransferMode::RemoteToLocal.to_string(), "remote → local");
        assert_eq!(
            TransferMode::RemoteToRemote.to_string(),
            "remote → remote"
        );
    }

    #[test]
    fn copy_stats_throughput() {
        let stats = CopyStats {
            bytes_written: 1_000_000,
            elapsed: Duration::from_secs(2),
            ..CopyStats::default()
        };
        let tp = stats.throughput_bps();
        assert!(tp.is_some());
        if let Some(bps) = tp {
            assert!((bps - 500_000.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn copy_stats_throughput_zero_elapsed() {
        let stats = CopyStats::default();
        assert!(stats.throughput_bps().is_none());
    }

    #[test]
    fn dedup_group_debug() {
        let group = DedupGroup {
            canonical: 0,
            duplicates: vec![1, 2, 3],
        };
        let debug = format!("{group:?}");
        assert!(debug.contains("canonical: 0"));
        assert!(debug.contains("duplicates"));
    }

    #[test]
    fn empty_copy_plan() {
        let plan = CopyPlan::new(PathBuf::from("/a"), PathBuf::from("/b"));
        assert_eq!(plan.total_bytes(), 0);
        assert_eq!(plan.link_count(), 0);
        assert!(plan.entries.is_empty());
        assert!(plan.dedup_groups.is_empty());
    }
}
