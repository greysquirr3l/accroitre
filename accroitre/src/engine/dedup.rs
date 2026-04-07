//! Content-aware deduplication engine.
//!
//! Groups files by content hash, producing a `CopyPlan` where each unique file
//! is copied once and duplicates are hard-linked.

use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

use crate::domain::{CopyPlan, DedupGroup, FileEntry, Hash, HashAlgorithm};
use crate::engine::hash::{HashConfig, hash_entries};
use crate::ports::{NullProgress, ProgressPort};

/// Statistics about the deduplication pass.
#[derive(Debug, Clone)]
pub struct DedupStats {
    /// Total files considered.
    pub total_files: u64,
    /// Number of unique files (will be copied).
    pub unique_files: u64,
    /// Number of duplicate files (will be hard-linked).
    pub duplicate_files: u64,
    /// Bytes saved by deduplication.
    pub bytes_saved: u64,
}

/// Build a `CopyPlan` from scanned & hashed file entries.
///
/// Two-pass strategy:
/// 1. Group by size — files with unique sizes can't be duplicates, so they
///    skip hashing entirely.
/// 2. Hash only size-collision groups over rayon, then group by hash.
///
/// # Errors
///
/// This function does not return a top-level error. Per-file hash errors are
/// silently skipped (those files are treated as unique).
pub fn build_dedup_plan(
    entries: Vec<FileEntry>,
    source_root: &Path,
    dest_root: &Path,
    hash_config: &HashConfig,
    progress: &dyn ProgressPort,
) -> (CopyPlan, DedupStats) {
    // Pass 1: group by size.
    let mut size_groups: HashMap<u64, Vec<FileEntry>> = HashMap::new();
    for entry in entries {
        size_groups.entry(entry.size).or_default().push(entry);
    }

    let mut unique_entries: Vec<FileEntry> = Vec::new();
    let mut needs_hashing: Vec<FileEntry> = Vec::new();

    for group in size_groups.values_mut() {
        if group.len() == 1 {
            // Unique size — no need to hash, just copy.
            if let Some(entry) = group.pop() {
                unique_entries.push(entry);
            }
        } else {
            // Size collision — need to hash to determine actual duplicates.
            needs_hashing.append(group);
        }
    }

    debug!(
        "dedup pass 1: {} unique-by-size, {} need hashing",
        unique_entries.len(),
        needs_hashing.len()
    );

    // Pass 2: hash the size-collision entries.
    let _hash_result = hash_entries(&mut needs_hashing, hash_config, progress);

    // Group hashed entries by hash.
    let mut hash_groups: HashMap<Hash, Vec<FileEntry>> = HashMap::new();
    let mut unhashed: Vec<FileEntry> = Vec::new();

    for entry in needs_hashing {
        if let Some(hash) = entry.hash.clone() {
            hash_groups.entry(hash).or_default().push(entry);
        } else {
            // Failed to hash — treat as unique.
            unhashed.push(entry);
        }
    }

    // Build the copy plan.
    let mut plan = CopyPlan::new(source_root.to_path_buf(), dest_root.to_path_buf());
    let mut bytes_saved: u64 = 0;
    let mut duplicate_count: u64 = 0;

    // Add unique-by-size entries (no dedup group needed).
    for entry in unique_entries {
        plan.entries.push(entry);
    }

    // Add unhashed entries (treated as unique).
    for entry in unhashed {
        plan.entries.push(entry);
    }

    // Add hash groups — the first file is canonical, rest are duplicates.
    for (_hash, group) in hash_groups {
        if group.len() == 1 {
            // Unique by content too — just add it.
            for entry in group {
                plan.entries.push(entry);
            }
        } else {
            let canonical_idx = plan.entries.len();
            let mut duplicate_indices = Vec::new();

            for (i, entry) in group.into_iter().enumerate() {
                let idx = plan.entries.len();
                if i > 0 {
                    bytes_saved += entry.size;
                    duplicate_count += 1;
                    duplicate_indices.push(idx);
                }
                plan.entries.push(entry);
            }

            plan.dedup_groups.push(DedupGroup {
                canonical: canonical_idx,
                duplicates: duplicate_indices,
            });
        }
    }

    let total_files = plan.entries.len() as u64;
    let unique_files = total_files - duplicate_count;

    debug!(
        "dedup complete: {total_files} total, {unique_files} unique, {duplicate_count} dupes, {} bytes saved",
        bytes_saved
    );

    let stats = DedupStats {
        total_files,
        unique_files,
        duplicate_files: duplicate_count,
        bytes_saved,
    };

    (plan, stats)
}

/// Convenience wrapper that hashes entries first before deduplication.
///
/// # Errors
///
/// Does not return top-level errors. Hash/dedup errors handled internally.
#[must_use]
pub fn dedup_with_hashing(
    entries: Vec<FileEntry>,
    source_root: &Path,
    dest_root: &Path,
    algorithm: HashAlgorithm,
) -> (CopyPlan, DedupStats) {
    let config = HashConfig {
        algorithm,
        ..HashConfig::default()
    };
    build_dedup_plan(entries, source_root, dest_root, &config, &NullProgress)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::domain::FileEntry;
    use crate::ports::NullProgress;

    fn make_entry(path: PathBuf, size: u64) -> FileEntry {
        FileEntry::new(path, size)
    }

    #[test]
    fn identical_files_produce_one_copy_and_one_link() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        let content = "identical content here";
        fs::write(root.join("a.txt"), content).expect("write");
        fs::write(root.join("b.txt"), content).expect("write");

        let entries = vec![
            make_entry(root.join("a.txt"), content.len() as u64),
            make_entry(root.join("b.txt"), content.len() as u64),
        ];

        let config = HashConfig::default();
        let (plan, stats) =
            build_dedup_plan(entries, root, Path::new("/dest"), &config, &NullProgress);

        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.dedup_groups.len(), 1);
        assert_eq!(stats.duplicate_files, 1);
        assert_eq!(stats.unique_files, 1);
        assert_eq!(stats.bytes_saved, content.len() as u64);
    }

    #[test]
    fn same_size_different_content_both_copied() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        // Same length (7 bytes) but different content.
        fs::write(root.join("a.txt"), "hello_a").expect("write");
        fs::write(root.join("b.txt"), "hello_b").expect("write");

        let entries = vec![
            make_entry(root.join("a.txt"), 7),
            make_entry(root.join("b.txt"), 7),
        ];

        let config = HashConfig::default();
        let (plan, stats) =
            build_dedup_plan(entries, root, Path::new("/dest"), &config, &NullProgress);

        assert_eq!(plan.entries.len(), 2);
        // No dedup groups since content differs.
        assert_eq!(plan.dedup_groups.len(), 0);
        assert_eq!(stats.duplicate_files, 0);
        assert_eq!(stats.unique_files, 2);
        assert_eq!(stats.bytes_saved, 0);
    }

    #[test]
    fn single_unique_file_no_hashing_overhead() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        fs::write(root.join("only.txt"), "unique file").expect("write");

        let entries = vec![make_entry(root.join("only.txt"), 11)];

        let config = HashConfig::default();
        let (plan, stats) =
            build_dedup_plan(entries, root, Path::new("/dest"), &config, &NullProgress);

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.dedup_groups.len(), 0);
        assert_eq!(stats.duplicate_files, 0);
        assert_eq!(stats.unique_files, 1);

        // The entry should NOT have been hashed (unique by size).
        assert!(plan.entries.first().is_some_and(|e| e.hash.is_none()));
    }

    #[test]
    fn multiple_duplicate_groups() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        // Group 1: 3 identical files.
        fs::write(root.join("g1_a.txt"), "group one content").expect("write");
        fs::write(root.join("g1_b.txt"), "group one content").expect("write");
        fs::write(root.join("g1_c.txt"), "group one content").expect("write");

        // Group 2: 2 identical files (different content from group 1 but same length).
        // Use different length to make a separate size group.
        fs::write(root.join("g2_a.txt"), "group two").expect("write");
        fs::write(root.join("g2_b.txt"), "group two").expect("write");

        // Unique file.
        fs::write(root.join("unique.txt"), "I am unique and alone here!").expect("write");

        let entries = vec![
            make_entry(root.join("g1_a.txt"), 17),
            make_entry(root.join("g1_b.txt"), 17),
            make_entry(root.join("g1_c.txt"), 17),
            make_entry(root.join("g2_a.txt"), 9),
            make_entry(root.join("g2_b.txt"), 9),
            make_entry(root.join("unique.txt"), 27),
        ];

        let config = HashConfig::default();
        let (plan, stats) =
            build_dedup_plan(entries, root, Path::new("/dest"), &config, &NullProgress);

        assert_eq!(plan.entries.len(), 6);
        assert_eq!(stats.total_files, 6);
        assert_eq!(stats.duplicate_files, 3); // 2 dupes from g1, 1 dupe from g2
        assert_eq!(stats.unique_files, 3); // 1 canonical from g1, 1 from g2, 1 unique
    }

    #[test]
    fn different_sizes_skip_hashing() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        fs::write(root.join("small.txt"), "hi").expect("write");
        fs::write(root.join("big.txt"), "much bigger content here").expect("write");

        let entries = vec![
            make_entry(root.join("small.txt"), 2),
            make_entry(root.join("big.txt"), 24),
        ];

        let config = HashConfig::default();
        let (plan, stats) =
            build_dedup_plan(entries, root, Path::new("/dest"), &config, &NullProgress);

        assert_eq!(plan.entries.len(), 2);
        assert_eq!(stats.duplicate_files, 0);

        // Neither should be hashed (unique by size).
        for entry in &plan.entries {
            assert!(entry.hash.is_none());
        }
    }

    #[test]
    fn empty_entries_produces_empty_plan() {
        let entries: Vec<FileEntry> = Vec::new();
        let config = HashConfig::default();
        let (plan, stats) = build_dedup_plan(
            entries,
            Path::new("/src"),
            Path::new("/dest"),
            &config,
            &NullProgress,
        );

        assert!(plan.entries.is_empty());
        assert!(plan.dedup_groups.is_empty());
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.bytes_saved, 0);
    }

    #[test]
    fn dedup_with_hashing_convenience() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();

        fs::write(root.join("a.txt"), "same").expect("write");
        fs::write(root.join("b.txt"), "same").expect("write");

        let entries = vec![
            make_entry(root.join("a.txt"), 4),
            make_entry(root.join("b.txt"), 4),
        ];

        let (plan, stats) =
            dedup_with_hashing(entries, root, Path::new("/dest"), HashAlgorithm::Blake3);

        assert_eq!(plan.entries.len(), 2);
        assert_eq!(stats.duplicate_files, 1);
    }
}
