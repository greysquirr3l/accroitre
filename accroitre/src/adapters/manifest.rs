//! Resumable copy manifest tracking.
//!
//! Tracks which files have been copied, verified, and linked. The manifest is
//! persisted as `.accroitre-manifest.json` at the destination root and updated
//! atomically (write-to-temp, rename) to survive crashes.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Name of the manifest file at the destination root.
const MANIFEST_FILENAME: &str = ".accroitre-manifest.json";

/// Status of an individual file in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// File has been copied to the destination.
    Copied,
    /// File has been copied and verified (hash matches).
    Verified,
    /// File is a hard-link to a canonical copy.
    Linked,
}

/// Record for a single file in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Relative path from the source root.
    pub relative_path: String,
    /// File size in bytes.
    pub size: u64,
    /// Hash of the source file (hex string), if available.
    pub source_hash: Option<String>,
    /// Current status of this file.
    pub status: FileStatus,
    /// Timestamp when this entry was last updated.
    pub updated_at: DateTime<Utc>,
}

/// The copy manifest, persisted at the destination root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyManifest {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Source root that was being copied.
    pub source_root: String,
    /// Destination root where files are being copied to.
    pub dest_root: String,
    /// Hash algorithm used (e.g. "xxhash128", "blake3").
    pub hash_algorithm: Option<String>,
    /// Timestamp when the copy started.
    pub started_at: DateTime<Utc>,
    /// Timestamp of last update.
    pub updated_at: DateTime<Utc>,
    /// Per-file tracking, keyed by relative path.
    pub files: HashMap<String, ManifestEntry>,
}

impl CopyManifest {
    /// Create a new empty manifest for a copy operation.
    #[must_use]
    pub fn new(source_root: &Path, dest_root: &Path, hash_algorithm: Option<&str>) -> Self {
        let now = Utc::now();
        Self {
            version: 1,
            source_root: source_root.to_string_lossy().into_owned(),
            dest_root: dest_root.to_string_lossy().into_owned(),
            hash_algorithm: hash_algorithm.map(String::from),
            started_at: now,
            updated_at: now,
            files: HashMap::new(),
        }
    }

    /// Get the manifest file path for a given destination root.
    #[must_use]
    pub fn manifest_path(dest_root: &Path) -> PathBuf {
        dest_root.join(MANIFEST_FILENAME)
    }

    /// Load an existing manifest from the destination root.
    ///
    /// Returns `Ok(None)` if no manifest exists.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the manifest file exists but cannot be read
    /// or parsed.
    pub fn load(dest_root: &Path) -> Result<Option<Self>, io::Error> {
        let path = Self::manifest_path(dest_root);
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)?;
        let manifest: Self = serde_json::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid manifest at {}: {e}", path.display()),
            )
        })?;

        debug!("loaded manifest with {} entries", manifest.files.len());
        Ok(Some(manifest))
    }

    /// Save the manifest atomically (write to temp file, then rename).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the manifest cannot be written.
    pub fn save(&mut self, dest_root: &Path) -> Result<(), io::Error> {
        self.updated_at = Utc::now();

        let path = Self::manifest_path(dest_root);
        let tmp_path = path.with_extension("json.tmp");

        let content = serde_json::to_string_pretty(self).map_err(|e| {
            io::Error::other(
                format!("failed to serialize manifest: {e}"),
            )
        })?;

        fs::write(&tmp_path, content)?;
        fs::rename(&tmp_path, &path)?;

        debug!("saved manifest with {} entries", self.files.len());
        Ok(())
    }

    /// Record that a file has been copied.
    pub fn mark_copied(
        &mut self,
        relative_path: &str,
        size: u64,
        source_hash: Option<&str>,
    ) {
        let entry = ManifestEntry {
            relative_path: relative_path.to_string(),
            size,
            source_hash: source_hash.map(String::from),
            status: FileStatus::Copied,
            updated_at: Utc::now(),
        };
        self.files.insert(relative_path.to_string(), entry);
    }

    /// Record that a file has been verified.
    pub fn mark_verified(&mut self, relative_path: &str) {
        if let Some(entry) = self.files.get_mut(relative_path) {
            entry.status = FileStatus::Verified;
            entry.updated_at = Utc::now();
        }
    }

    /// Record that a file has been hard-linked.
    pub fn mark_linked(
        &mut self,
        relative_path: &str,
        size: u64,
        source_hash: Option<&str>,
    ) {
        let entry = ManifestEntry {
            relative_path: relative_path.to_string(),
            size,
            source_hash: source_hash.map(String::from),
            status: FileStatus::Linked,
            updated_at: Utc::now(),
        };
        self.files.insert(relative_path.to_string(), entry);
    }

    /// Check if a file has already been completed (copied, verified, or linked).
    ///
    /// Validates that the source file hasn't changed by comparing size and hash.
    #[must_use]
    pub fn is_completed(
        &self,
        relative_path: &str,
        size: u64,
        source_hash: Option<&str>,
    ) -> bool {
        let Some(entry) = self.files.get(relative_path) else {
            return false;
        };

        // Size must match.
        if entry.size != size {
            debug!(
                "size mismatch for {relative_path}: manifest={}, current={size}",
                entry.size
            );
            return false;
        }

        // If both have hashes, they must match.
        if let (Some(manifest_hash), Some(current_hash)) =
            (&entry.source_hash, source_hash)
            && manifest_hash != current_hash
        {
            debug!("hash mismatch for {relative_path}");
            return false;
        }

        true
    }

    /// Return the count of completed files.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.files.len()
    }

    /// Remove the manifest file from the destination root.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the file exists but cannot be removed.
    pub fn remove(dest_root: &Path) -> Result<(), io::Error> {
        let path = Self::manifest_path(dest_root);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn new_manifest_has_correct_defaults() {
        let m = CopyManifest::new(
            Path::new("/src"),
            Path::new("/dst"),
            Some("xxhash128"),
        );
        assert_eq!(m.version, 1);
        assert_eq!(m.source_root, "/src");
        assert_eq!(m.dest_root, "/dst");
        assert_eq!(m.hash_algorithm.as_deref(), Some("xxhash128"));
        assert!(m.files.is_empty());
    }

    #[test]
    fn mark_and_check_copied() {
        let mut m = CopyManifest::new(
            Path::new("/src"),
            Path::new("/dst"),
            None,
        );
        m.mark_copied("dir/file.txt", 1024, Some("abc123"));
        assert!(m.is_completed("dir/file.txt", 1024, Some("abc123")));
        assert!(!m.is_completed("dir/file.txt", 2048, Some("abc123"))); // size changed
        assert!(!m.is_completed("dir/file.txt", 1024, Some("def456"))); // hash changed
        assert!(!m.is_completed("other.txt", 1024, Some("abc123"))); // different file
    }

    #[test]
    fn mark_verified_updates_status() {
        let mut m = CopyManifest::new(
            Path::new("/src"),
            Path::new("/dst"),
            None,
        );
        m.mark_copied("file.txt", 100, None);
        assert_eq!(m.files.get("file.txt").map(|e| &e.status), Some(&FileStatus::Copied));

        m.mark_verified("file.txt");
        assert_eq!(m.files.get("file.txt").map(|e| &e.status), Some(&FileStatus::Verified));
    }

    #[test]
    fn mark_linked() {
        let mut m = CopyManifest::new(
            Path::new("/src"),
            Path::new("/dst"),
            None,
        );
        m.mark_linked("dup.txt", 500, Some("hash1"));
        assert_eq!(m.files.get("dup.txt").map(|e| &e.status), Some(&FileStatus::Linked));
        assert!(m.is_completed("dup.txt", 500, Some("hash1")));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().expect("test tmpdir");
        let dest = dir.path();

        let mut m = CopyManifest::new(
            Path::new("/source"),
            dest,
            Some("blake3"),
        );
        m.mark_copied("a.txt", 100, Some("hash_a"));
        m.mark_linked("b.txt", 200, Some("hash_b"));
        m.save(dest).expect("save should succeed");

        let loaded = CopyManifest::load(dest)
            .expect("load should succeed")
            .expect("manifest should exist");
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.files.len(), 2);
        assert!(loaded.is_completed("a.txt", 100, Some("hash_a")));
        assert!(loaded.is_completed("b.txt", 200, Some("hash_b")));
    }

    #[test]
    fn load_returns_none_when_no_manifest() {
        let dir = TempDir::new().expect("test tmpdir");
        let result = CopyManifest::load(dir.path()).expect("load should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn atomic_save_survives_read_after_write() {
        let dir = TempDir::new().expect("test tmpdir");
        let dest = dir.path();

        let mut m = CopyManifest::new(Path::new("/s"), dest, None);
        m.mark_copied("f1.txt", 10, None);
        m.save(dest).expect("first save");

        // Update and save again.
        m.mark_copied("f2.txt", 20, None);
        m.save(dest).expect("second save");

        let loaded = CopyManifest::load(dest)
            .expect("load")
            .expect("manifest exists");
        assert_eq!(loaded.files.len(), 2);
    }

    #[test]
    fn remove_deletes_manifest() {
        let dir = TempDir::new().expect("test tmpdir");
        let dest = dir.path();

        let mut m = CopyManifest::new(Path::new("/s"), dest, None);
        m.save(dest).expect("save");
        assert!(CopyManifest::manifest_path(dest).exists());

        CopyManifest::remove(dest).expect("remove");
        assert!(!CopyManifest::manifest_path(dest).exists());
    }

    #[test]
    fn remove_nonexistent_is_ok() {
        let dir = TempDir::new().expect("test tmpdir");
        CopyManifest::remove(dir.path()).expect("remove nonexistent should succeed");
    }

    #[test]
    fn is_completed_without_hash_only_checks_size() {
        let mut m = CopyManifest::new(Path::new("/s"), Path::new("/d"), None);
        m.mark_copied("file.txt", 100, None);
        // No hash on either side — only size matters.
        assert!(m.is_completed("file.txt", 100, None));
        assert!(!m.is_completed("file.txt", 200, None));
    }

    #[test]
    fn completed_count_tracks_entries() {
        let mut m = CopyManifest::new(Path::new("/s"), Path::new("/d"), None);
        assert_eq!(m.completed_count(), 0);
        m.mark_copied("a.txt", 10, None);
        assert_eq!(m.completed_count(), 1);
        m.mark_linked("b.txt", 20, None);
        assert_eq!(m.completed_count(), 2);
    }
}
