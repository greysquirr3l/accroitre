//! Post-copy verification engine.
//!
//! Recomputes the hash of each copied file on the destination and compares
//! it against the source hash stored in the plan. Hard-linked duplicates
//! are verified only once (they share the same inode as the canonical copy).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use tracing::warn;

use crate::domain::{CopyPlan, DedupGroup, HashAlgorithm, VerifyError};
use crate::engine::copy::map_source_to_dest;
use crate::engine::hash::hash_file;
use crate::ports::{ProgressPort, ProgressUpdate};

/// Configuration for the verification engine.
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    /// Hash algorithm to use for verification (must match the one used during hashing).
    pub algorithm: HashAlgorithm,
    /// Read buffer size in bytes for hashing.
    pub buffer_size: usize,
    /// Number of rayon threads (None = rayon default).
    pub thread_count: Option<usize>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            algorithm: HashAlgorithm::XxHash128,
            buffer_size: 64 * 1024,
            thread_count: None,
        }
    }
}

/// Result of a verification operation.
#[derive(Debug)]
pub struct VerifyResult {
    /// Number of files successfully verified.
    pub files_verified: u64,
    /// Number of files that failed verification.
    pub files_failed: u64,
    /// Number of duplicate files skipped (shared inode with canonical).
    pub duplicates_skipped: u64,
    /// Detailed failures.
    pub failures: Vec<VerifyError>,
}

/// Verify all files in a copy plan by recomputing destination hashes.
///
/// Hard-linked duplicates (listed in `dedup_groups`) are skipped — only
/// the canonical copy is verified since links share the same inode.
///
/// # Errors
///
/// Per-file errors are collected in `VerifyResult.failures` rather than
/// aborting the whole operation.
pub fn verify_plan(
    plan: &CopyPlan,
    config: &VerifyConfig,
    progress: &dyn ProgressPort,
) -> VerifyResult {
    // Build set of duplicate indices — these share an inode with their
    // canonical copy and don't need independent verification.
    let duplicate_indices = build_duplicate_set(&plan.dedup_groups);
    let duplicates_skipped = duplicate_indices.len() as u64;

    // Collect (index, expected_hash, dest_path) for files to verify.
    let verify_list: Vec<_> = plan
        .entries
        .iter()
        .enumerate()
        .filter(|(idx, _)| !duplicate_indices.contains(idx))
        .filter_map(|(idx, entry)| {
            let expected = entry.hash.as_ref()?;
            let dest = map_source_to_dest(&entry.path, &plan.source_root, &plan.dest_root);
            Some((idx, expected.clone(), dest, entry.size))
        })
        .collect();

    let files_total = verify_list.len() as u64 + duplicates_skipped;
    let verified_count = AtomicU64::new(0);

    // Build a thread pool if a custom thread count is specified.
    let pool = config.thread_count.and_then(|n| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .ok()
    });

    let do_verify = || {
        verify_list
            .par_iter()
            .filter_map(|(_idx, expected, dest_path, expected_size)| {
                // Check file exists.
                let Ok(meta) = std::fs::metadata(dest_path) else {
                    let done = verified_count.fetch_add(1, Ordering::Relaxed) + 1;
                    progress.update(&ProgressUpdate::VerifyProgress {
                        files_verified: done,
                        files_total,
                    });
                    return Some(VerifyError::MissingFile(dest_path.clone()));
                };

                // Check size first (fast).
                if meta.len() != *expected_size {
                    let done = verified_count.fetch_add(1, Ordering::Relaxed) + 1;
                    progress.update(&ProgressUpdate::VerifyProgress {
                        files_verified: done,
                        files_total,
                    });
                    return Some(VerifyError::SizeMismatch {
                        path: dest_path.clone(),
                        expected: *expected_size,
                        actual: meta.len(),
                    });
                }

                // Recompute hash.
                let actual_hash =
                    match hash_file(dest_path, config.algorithm, config.buffer_size) {
                        Ok(h) => h,
                        Err(e) => {
                            warn!("could not hash {}: {e}", dest_path.display());
                            let done = verified_count.fetch_add(1, Ordering::Relaxed) + 1;
                            progress.update(&ProgressUpdate::VerifyProgress {
                                files_verified: done,
                                files_total,
                            });
                            return Some(VerifyError::Io {
                                path: dest_path.clone(),
                                source: std::io::Error::other(e.to_string()),
                            });
                        }
                    };

                let done = verified_count.fetch_add(1, Ordering::Relaxed) + 1;
                progress.update(&ProgressUpdate::VerifyProgress {
                    files_verified: done,
                    files_total,
                });

                if actual_hash == *expected {
                    None
                } else {
                    Some(VerifyError::HashMismatch {
                        path: dest_path.clone(),
                        expected: expected.to_string(),
                        actual: actual_hash.to_string(),
                    })
                }
            })
            .collect::<Vec<_>>()
    };

    let failures: Vec<VerifyError> = match pool {
        Some(ref p) => p.install(do_verify),
        None => do_verify(),
    };

    let files_failed = failures.len() as u64;
    let files_verified = verified_count.load(Ordering::Relaxed) - files_failed;

    progress.update(&ProgressUpdate::PhaseComplete { phase: "verify" });

    VerifyResult {
        files_verified,
        files_failed,
        duplicates_skipped,
        failures,
    }
}

/// Build a set of all duplicate file indices from dedup groups.
fn build_duplicate_set(groups: &[DedupGroup]) -> HashSet<usize> {
    let mut set = HashSet::new();
    for group in groups {
        for &dup_idx in &group.duplicates {
            set.insert(dup_idx);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CopyPlan, DedupGroup, FileEntry, HashAlgorithm};
    use crate::engine::hash::hash_bytes;
    use crate::ports::NullProgress;
    use std::fs;
    use tempfile::TempDir;

    fn make_file(dir: &std::path::Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&path, content).expect("write test file");
        path
    }

    #[test]
    fn verification_passes_for_correct_copies() {
        let src_dir = TempDir::new().expect("tempdir");
        let dst_dir = TempDir::new().expect("tempdir");

        let content = b"hello verification";
        let src_path = make_file(src_dir.path(), "a.txt", content);
        let dst_path = make_file(dst_dir.path(), "a.txt", content);
        let _ = dst_path;

        let hash = hash_bytes(content, HashAlgorithm::XxHash128);

        let plan = CopyPlan {
            source_root: src_dir.path().to_path_buf(),
            dest_root: dst_dir.path().to_path_buf(),
            entries: vec![FileEntry {
                path: src_path,
                size: content.len() as u64,
                hash: Some(hash),
                physical_offset: None,
                permissions: 0,
            }],
            dedup_groups: vec![],
        };

        let config = VerifyConfig::default();
        let progress = NullProgress;
        let result = verify_plan(&plan, &config, &progress);

        assert_eq!(result.files_verified, 1);
        assert_eq!(result.files_failed, 0);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn verification_detects_corrupted_file() {
        let src_dir = TempDir::new().expect("tempdir");
        let dst_dir = TempDir::new().expect("tempdir");

        let original = b"original content";
        let corrupted = b"CORRUPTED content";
        let src_path = make_file(src_dir.path(), "b.txt", original);
        let _dst_path = make_file(dst_dir.path(), "b.txt", corrupted);

        let hash = hash_bytes(original, HashAlgorithm::XxHash128);

        let plan = CopyPlan {
            source_root: src_dir.path().to_path_buf(),
            dest_root: dst_dir.path().to_path_buf(),
            entries: vec![FileEntry {
                path: src_path,
                size: original.len() as u64,
                hash: Some(hash),
                physical_offset: None,
                permissions: 0,
            }],
            dedup_groups: vec![],
        };

        let config = VerifyConfig::default();
        let progress = NullProgress;
        let result = verify_plan(&plan, &config, &progress);

        assert_eq!(result.files_failed, 1);
        assert!(!result.failures.is_empty());
        assert!(
            matches!(&result.failures[0], VerifyError::SizeMismatch { .. } | VerifyError::HashMismatch { .. })
        );
    }

    #[test]
    fn hard_linked_files_verified_only_once() {
        let src_dir = TempDir::new().expect("tempdir");
        let dst_dir = TempDir::new().expect("tempdir");

        let content = b"duplicate content";
        let src_a = make_file(src_dir.path(), "a.txt", content);
        let src_b = make_file(src_dir.path(), "b.txt", content);
        let _dst_a = make_file(dst_dir.path(), "a.txt", content);

        // Create hard link for b -> a at destination.
        let dst_b = dst_dir.path().join("b.txt");
        fs::hard_link(dst_dir.path().join("a.txt"), &dst_b).expect("hard link");

        let hash = hash_bytes(content, HashAlgorithm::XxHash128);

        let plan = CopyPlan {
            source_root: src_dir.path().to_path_buf(),
            dest_root: dst_dir.path().to_path_buf(),
            entries: vec![
                FileEntry {
                    path: src_a,
                    size: content.len() as u64,
                    hash: Some(hash.clone()),
                    physical_offset: None,
                    permissions: 0,
                },
                FileEntry {
                    path: src_b,
                    size: content.len() as u64,
                    hash: Some(hash),
                    physical_offset: None,
                    permissions: 0,
                },
            ],
            dedup_groups: vec![DedupGroup {
                canonical: 0,
                duplicates: vec![1],
            }],
        };

        let config = VerifyConfig::default();
        let progress = NullProgress;
        let result = verify_plan(&plan, &config, &progress);

        // Only canonical (index 0) should be verified; index 1 is skipped.
        assert_eq!(result.files_verified, 1);
        assert_eq!(result.duplicates_skipped, 1);
        assert_eq!(result.files_failed, 0);
    }
}
