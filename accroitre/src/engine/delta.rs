//! Delta synchronization — compare source and destination to copy only
//! new or changed files.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::domain::FileEntry;

/// Result of a delta comparison between source and destination.
#[derive(Debug)]
pub struct DeltaResult {
    /// Files that need to be copied (new or changed).
    pub changed: Vec<FileEntry>,
    /// Destination paths that have no corresponding source file.
    pub orphans: Vec<PathBuf>,
    /// Files that are unchanged (skipped).
    pub unchanged_count: u64,
}

/// Compare source entries against the destination to determine which files
/// need copying.
///
/// A file is considered unchanged if:
/// - It exists at the destination with the same size and modification time.
///
/// A file is considered changed if:
/// - It doesn't exist at the destination, or
/// - Its size differs, or
/// - Its modification time differs.
#[must_use]
pub fn compute_delta(
    source_entries: Vec<FileEntry>,
    source_root: &Path,
    dest_root: &Path,
) -> DeltaResult {
    let mut changed = Vec::new();
    let mut unchanged_count: u64 = 0;

    for entry in source_entries {
        let relative = entry.path.strip_prefix(source_root).unwrap_or(&entry.path);
        let dest_path = dest_root.join(relative);

        if is_unchanged(&entry, &dest_path) {
            unchanged_count += 1;
            debug!("unchanged: {}", relative.display());
        } else {
            changed.push(entry);
        }
    }

    info!(
        "delta: {} changed, {} unchanged",
        changed.len(),
        unchanged_count
    );

    DeltaResult {
        changed,
        orphans: Vec::new(),
        unchanged_count,
    }
}

/// Compare source entries and scan the destination to find orphaned files
/// that should be deleted.
///
/// # Errors
///
/// Returns `Ok` even if individual destination files can't be read — those
/// are simply skipped.
pub fn find_orphans(
    source_entries: &[FileEntry],
    source_root: &Path,
    dest_root: &Path,
) -> Vec<PathBuf> {
    let source_relatives: HashSet<PathBuf> = source_entries
        .iter()
        .filter_map(|e| e.path.strip_prefix(source_root).ok().map(PathBuf::from))
        .collect();

    let mut orphans = Vec::new();
    collect_dest_files(dest_root, dest_root, &source_relatives, &mut orphans);

    info!("found {} orphaned destination files", orphans.len());
    orphans
}

/// Delete orphaned destination files.
///
/// Returns the number of files successfully deleted.
pub fn delete_orphans(orphans: &[PathBuf]) -> u64 {
    let mut deleted = 0u64;

    for path in orphans {
        match fs::remove_file(path) {
            Ok(()) => {
                debug!("deleted orphan: {}", path.display());
                deleted += 1;
            }
            Err(e) => {
                warn!("failed to delete orphan {}: {e}", path.display());
            }
        }
    }

    if deleted > 0 {
        info!("deleted {deleted} orphaned files");
    }

    deleted
}

/// Check if a source file is unchanged at the destination.
fn is_unchanged(source: &FileEntry, dest_path: &Path) -> bool {
    let Ok(dest_meta) = fs::metadata(dest_path) else {
        return false;
    };

    if dest_meta.len() != source.size {
        return false;
    }

    // Compare mtime if available.
    if let Some(src_mtime) = source.modified_epoch {
        let dest_mtime = dest_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        if let Some(dst_mtime) = dest_mtime {
            return src_mtime == dst_mtime;
        }
    }

    // If mtime unavailable on either side, treat as changed.
    false
}

/// Recursively collect destination files, skipping accroitre metadata.
fn collect_dest_files(
    dir: &Path,
    dest_root: &Path,
    source_relatives: &HashSet<PathBuf>,
    orphans: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip accroitre metadata files.
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(".accroitre-")
        {
            continue;
        }

        if path.is_dir() {
            collect_dest_files(&path, dest_root, source_relatives, orphans);
        } else if path.is_file()
            && let Ok(relative) = path.strip_prefix(dest_root)
            && !source_relatives.contains(relative)
        {
            orphans.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_entry(path: &Path, size: u64, mtime: Option<u64>) -> FileEntry {
        FileEntry {
            path: path.to_path_buf(),
            size,
            hash: None,
            physical_offset: None,
            permissions: 0o644,
            modified_epoch: mtime,
        }
    }

    #[test]
    fn new_files_are_changed() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        fs::write(src.join("new.txt"), "hello")?;

        let entries = vec![make_entry(&src.join("new.txt"), 5, Some(1_000_000))];
        let result = compute_delta(entries, &src, &dst);

        assert_eq!(result.changed.len(), 1);
        assert_eq!(result.unchanged_count, 0);
        Ok(())
    }

    #[test]
    fn unchanged_files_are_skipped() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        fs::write(src.join("same.txt"), "hello")?;
        fs::write(dst.join("same.txt"), "hello")?;

        // Get actual mtime from the destination file.
        let meta = fs::metadata(dst.join("same.txt"))?;
        let mtime = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        let entries = vec![make_entry(&src.join("same.txt"), 5, Some(mtime))];
        let result = compute_delta(entries, &src, &dst);

        assert_eq!(result.changed.len(), 0);
        assert_eq!(result.unchanged_count, 1);
        Ok(())
    }

    #[test]
    fn size_change_is_detected() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        fs::write(src.join("changed.txt"), "longer content")?;
        fs::write(dst.join("changed.txt"), "short")?;

        let meta = fs::metadata(dst.join("changed.txt"))?;
        let mtime = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        // Source reports different size.
        let entries = vec![make_entry(&src.join("changed.txt"), 14, Some(mtime))];
        let result = compute_delta(entries, &src, &dst);

        assert_eq!(result.changed.len(), 1);
        Ok(())
    }

    #[test]
    fn orphans_found_and_deleted() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        // Source has only one file.
        fs::write(src.join("keep.txt"), "keep")?;
        // Destination has the file plus an orphan.
        fs::write(dst.join("keep.txt"), "keep")?;
        fs::write(dst.join("orphan.txt"), "go away")?;

        let entries = vec![make_entry(&src.join("keep.txt"), 4, Some(1_000_000))];
        let orphans = find_orphans(&entries, &src, &dst);

        assert_eq!(orphans.len(), 1);
        let first = orphans.first().ok_or("orphans was empty")?;
        assert!(first.ends_with("orphan.txt"));

        let deleted = delete_orphans(&orphans);
        assert_eq!(deleted, 1);
        assert!(!dst.join("orphan.txt").exists());
        Ok(())
    }

    #[test]
    fn metadata_files_excluded_from_orphans() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        fs::write(dst.join(".accroitre-cache.db"), "db")?;
        fs::write(dst.join(".accroitre-manifest.json"), "{}")?;

        let orphans = find_orphans(&[], &src, &dst);
        assert!(orphans.is_empty());
        Ok(())
    }

    #[test]
    fn mtime_change_is_detected() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;

        fs::write(src.join("file.txt"), "hello")?;
        fs::write(dst.join("file.txt"), "hello")?;

        // Source reports a different mtime than what's on disk.
        let entries = vec![make_entry(&src.join("file.txt"), 5, Some(999_999))];
        let result = compute_delta(entries, &src, &dst);

        assert_eq!(result.changed.len(), 1);
        Ok(())
    }
}
