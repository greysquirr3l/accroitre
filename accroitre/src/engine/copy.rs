//! Block-order copy engine with large file buffers and tar streaming.
//!
//! Large files are copied with platform-optimal syscalls (`clonefile` on macOS
//! APFS, `copy_file_range` on Linux, buffered copy elsewhere). Small files are
//! batched into tar streams for reduced syscall overhead. Duplicates are
//! hard-linked after their canonical file is copied.
//!
//! Supports resumable copies via an optional manifest that tracks per-file
//! completion status. When a manifest is provided, already-completed files
//! are skipped and the manifest is updated atomically after each file.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::adapters::manifest::CopyManifest;
use crate::domain::{CopyError, CopyPlan};
use crate::ports::{ProgressPort, ProgressUpdate};

/// Default large-file buffer size (64 MiB).
const DEFAULT_BUFFER_SIZE: usize = 64 * 1024 * 1024;

/// Threshold below which files are considered "small" and batched into tar (1 MiB).
const SMALL_FILE_THRESHOLD: u64 = 1024 * 1024;

/// Configuration for the copy engine.
#[derive(Debug, Clone)]
pub struct CopyConfig {
    /// Buffer size for large file copies (default: 64 MiB).
    pub buffer_size: usize,
    /// Threshold below which files are tar-batched (default: 1 MiB).
    pub small_file_threshold: u64,
    /// Whether to attempt macOS APFS clonefile.
    pub try_clonefile: bool,
}

impl Default for CopyConfig {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_BUFFER_SIZE,
            small_file_threshold: SMALL_FILE_THRESHOLD,
            try_clonefile: cfg!(target_os = "macos"),
        }
    }
}

/// Result of a copy operation.
#[derive(Debug)]
pub struct CopyResult {
    /// Number of files copied.
    pub files_copied: u64,
    /// Number of files hard-linked.
    pub files_linked: u64,
    /// Total bytes copied.
    pub bytes_copied: u64,
    /// Non-fatal errors encountered.
    pub errors: Vec<CopyError>,
}

/// Execute a copy plan: copy unique files, hard-link duplicates.
///
/// # Errors
///
/// Returns `CopyError::InsufficientSpace` if the destination doesn't have
/// enough free space. Per-file errors are collected in `CopyResult.errors`.
pub fn execute_copy_plan(
    plan: &CopyPlan,
    config: &CopyConfig,
    progress: &dyn ProgressPort,
) -> Result<CopyResult, CopyError> {
    execute_copy_plan_resumable(plan, config, progress, None)
}

/// Execute a copy plan with optional resume support via a manifest.
///
/// When a manifest is provided, already-completed files are skipped and the
/// manifest is updated atomically after each file is copied or linked.
///
/// # Errors
///
/// Returns `CopyError::InsufficientSpace` if the destination doesn't have
/// enough free space. Per-file errors are collected in `CopyResult.errors`.
pub fn execute_copy_plan_resumable(
    plan: &CopyPlan,
    config: &CopyConfig,
    progress: &dyn ProgressPort,
    mut manifest: Option<&mut CopyManifest>,
) -> Result<CopyResult, CopyError> {
    // Pre-flight space check.
    let needed_bytes = estimate_copy_bytes(plan);
    check_free_space(&plan.dest_root, needed_bytes)?;

    let mut result = CopyResult {
        files_copied: 0,
        files_linked: 0,
        bytes_copied: 0,
        errors: Vec::new(),
    };

    // Track how many files were skipped from a previous run.
    let mut files_skipped: u64 = 0;

    let files_total = plan.entries.len() as u64;
    let bytes_total: u64 = plan.entries.iter().map(|e| e.size).sum();

    // Collect indices of duplicate files for quick lookup.
    let mut duplicate_indices = std::collections::HashSet::new();
    for group in &plan.dedup_groups {
        for &dup_idx in &group.duplicates {
            duplicate_indices.insert(dup_idx);
        }
    }

    // Phase 1: Copy unique/canonical files (in disk order — plan.entries is
    // already sorted by physical_offset from the scan engine).
    // Split into small-file batches and large files.
    let mut small_files: Vec<(usize, &Path, &Path)> = Vec::new();

    for (idx, entry) in plan.entries.iter().enumerate() {
        if duplicate_indices.contains(&idx) {
            continue; // Will be hard-linked in phase 2.
        }

        // Check manifest for already-completed files.
        let relative = relative_path_str(&entry.path, &plan.source_root);
        let hash_str = entry.hash.as_ref().map(std::string::ToString::to_string);
        if let Some(ref manifest) = manifest
            && manifest.is_completed(&relative, entry.size, hash_str.as_deref())
        {
            files_skipped += 1;
            debug!("skipping already-completed: {relative}");
            continue;
        }

        let dest_path = map_source_to_dest(&entry.path, &plan.source_root, &plan.dest_root);

        // Ensure parent directory exists.
        if let Some(parent) = dest_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            let err = CopyError::CreateDir {
                path: parent.to_path_buf(),
                source: e,
            };
            warn!("{err}");
            result.errors.push(err);
            continue;
        }

        if entry.size < config.small_file_threshold {
            small_files.push((idx, &entry.path, dest_path.leak()));
        } else {
            // Large file: copy with optimal syscall.
            match copy_large_file(&entry.path, &dest_path, config) {
                Ok(()) => {
                    // Preserve permissions.
                    preserve_permissions(&entry.path, &dest_path, entry.permissions);

                    result.files_copied += 1;
                    result.bytes_copied += entry.size;

                    // Update manifest for this file.
                    if let Some(ref mut m) = manifest {
                        m.mark_copied(&relative, entry.size, hash_str.as_deref());
                        if let Err(e) = m.save(&plan.dest_root) {
                            warn!("failed to update manifest: {e}");
                        }
                    }

                    progress.update(&ProgressUpdate::CopyProgress {
                        files_copied: result.files_copied + result.files_linked + files_skipped,
                        files_total,
                        bytes_copied: result.bytes_copied,
                        bytes_total,
                    });
                }
                Err(e) => {
                    warn!("{e}");
                    result.errors.push(e);
                }
            }
        }
    }

    // Batch-copy small files via tar stream.
    if !small_files.is_empty() {
        copy_small_files_tar(
            &small_files,
            &plan.source_root,
            &mut result,
            progress,
            files_total,
            bytes_total,
            plan,
        );
        // Update manifest for small files that were successfully copied.
        if let Some(ref mut m) = manifest {
            for &(idx, _, _) in &small_files {
                if let Some(entry) = plan.entries.get(idx) {
                    let rel = relative_path_str(&entry.path, &plan.source_root);
                    let h = entry.hash.as_ref().map(std::string::ToString::to_string);
                    m.mark_copied(&rel, entry.size, h.as_deref());
                }
            }
            if let Err(e) = m.save(&plan.dest_root) {
                warn!("failed to update manifest after small files: {e}");
            }
        }
    }

    // Phase 2: Hard-link duplicates.
    create_hard_links_resumable(plan, &mut result, progress, files_total, bytes_total, files_skipped, manifest);

    progress.update(&ProgressUpdate::PhaseComplete { phase: "copy" });

    if files_skipped > 0 {
        debug!("resumed: {files_skipped} files skipped from previous run");
    }

    Ok(result)
}

/// Create hard links for all duplicate files, with optional manifest tracking.
fn create_hard_links_resumable(
    plan: &CopyPlan,
    result: &mut CopyResult,
    progress: &dyn ProgressPort,
    files_total: u64,
    bytes_total: u64,
    files_skipped: u64,
    mut manifest: Option<&mut CopyManifest>,
) {
    for group in &plan.dedup_groups {
        let Some(canonical) = plan.entries.get(group.canonical) else {
            continue;
        };
        let canonical_dest =
            map_source_to_dest(&canonical.path, &plan.source_root, &plan.dest_root);

        for &dup_idx in &group.duplicates {
            let Some(dup_entry) = plan.entries.get(dup_idx) else {
                continue;
            };

            // Check manifest for already-completed links.
            let relative = relative_path_str(&dup_entry.path, &plan.source_root);
            let hash_str = dup_entry.hash.as_ref().map(std::string::ToString::to_string);
            if let Some(ref manifest) = manifest
                && manifest.is_completed(&relative, dup_entry.size, hash_str.as_deref())
            {
                debug!("skipping already-linked: {relative}");
                continue;
            }

            let dup_dest = map_source_to_dest(&dup_entry.path, &plan.source_root, &plan.dest_root);

            // Ensure parent directory exists.
            if let Some(parent) = dup_dest.parent()
                && let Err(e) = fs::create_dir_all(parent)
            {
                let err = CopyError::CreateDir {
                    path: parent.to_path_buf(),
                    source: e,
                };
                warn!("{err}");
                result.errors.push(err);
                continue;
            }

            match fs::hard_link(&canonical_dest, &dup_dest) {
                Ok(()) => {
                    result.files_linked += 1;

                    if let Some(ref mut m) = manifest {
                        m.mark_linked(&relative, dup_entry.size, hash_str.as_deref());
                        if let Err(e) = m.save(&plan.dest_root) {
                            warn!("failed to update manifest: {e}");
                        }
                    }

                    progress.update(&ProgressUpdate::CopyProgress {
                        files_copied: result.files_copied + result.files_linked + files_skipped,
                        files_total,
                        bytes_copied: result.bytes_copied,
                        bytes_total,
                    });
                }
                Err(e) => {
                    let err = CopyError::HardLink {
                        src: canonical_dest.clone(),
                        dst: dup_dest,
                        source: e,
                    };
                    warn!("{err}");
                    result.errors.push(err);
                }
            }
        }
    }
}

/// Map a source path to its corresponding destination path.
#[must_use]
pub fn map_source_to_dest(source_path: &Path, source_root: &Path, dest_root: &Path) -> PathBuf {
    let relative = source_path.strip_prefix(source_root).unwrap_or(source_path);
    dest_root.join(relative)
}

/// Get the relative path string of a file within a root directory.
fn relative_path_str(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Estimate how many bytes need to be physically copied (after dedup).
fn estimate_copy_bytes(plan: &CopyPlan) -> u64 {
    let mut duplicate_indices = std::collections::HashSet::new();
    for group in &plan.dedup_groups {
        for &dup_idx in &group.duplicates {
            duplicate_indices.insert(dup_idx);
        }
    }

    plan.entries
        .iter()
        .enumerate()
        .filter(|(idx, _)| !duplicate_indices.contains(idx))
        .map(|(_, e)| e.size)
        .sum()
}

/// Check available free space at the destination.
fn check_free_space(dest: &Path, needed: u64) -> Result<(), CopyError> {
    // Try to check the existing destination or its nearest existing ancestor.
    let check_path = find_existing_ancestor(dest);

    match get_available_space(&check_path) {
        Ok(available) => {
            if available < needed {
                Err(CopyError::InsufficientSpace { needed, available })
            } else {
                Ok(())
            }
        }
        Err(e) => {
            // If we can't determine space, log warning but proceed.
            warn!(
                "could not check free space at {}: {e}",
                check_path.display()
            );
            Ok(())
        }
    }
}

/// Walk up to find the nearest existing ancestor directory.
fn find_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    current
}

/// Get available disk space (platform-specific).
#[cfg(unix)]
fn get_available_space(path: &Path) -> Result<u64, io::Error> {
    use std::ffi::CString;

    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // SAFETY: statvfs is a standard POSIX call; we pass a valid C string and
    // read from a properly-initialized struct.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &raw mut stat) == 0 {
            // f_bavail * f_frsize = bytes available to non-root users.
            Ok(u64::from(stat.f_bavail) * stat.f_frsize)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(unix))]
fn get_available_space(_path: &Path) -> Result<u64, io::Error> {
    // On Windows, use the windows_io module; on other non-Unix, skip.
    #[cfg(target_os = "windows")]
    {
        return super::windows_io::get_available_space_windows(_path);
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(u64::MAX)
    }
}

/// Copy a large file using platform-optimal methods.
fn copy_large_file(src: &Path, dest: &Path, config: &CopyConfig) -> Result<(), CopyError> {
    // On macOS, try clonefile → fcopyfile → buffered copy.
    #[cfg(target_os = "macos")]
    if matches!(super::macos_io::try_macos_optimal_copy(src, dest, config.try_clonefile), Ok(true)) {
        return Ok(());
    }

    // On Linux, try copy_file_range (kernel-to-kernel zero-copy).
    #[cfg(target_os = "linux")]
    if let Ok(true) = super::linux_io::try_copy_file_range(src, dest) {
        return Ok(());
    }

    // On Windows, try ReFS block clone → CopyFileExW → buffered copy.
    #[cfg(target_os = "windows")]
    if let Ok(true) = super::windows_io::try_windows_optimal_copy(src, dest) {
        return Ok(());
    }

    // Suppress unused warning on platforms without optimised copy.
    let _ = config.try_clonefile;

    // Fall back to buffered copy.
    buffered_copy(src, dest, config.buffer_size)
}

/// Buffered file copy with configurable buffer size.
fn buffered_copy(src: &Path, dest: &Path, buffer_size: usize) -> Result<(), CopyError> {
    let mut reader = fs::File::open(src).map_err(|e| CopyError::FileCopy {
        src: src.to_path_buf(),
        dst: dest.to_path_buf(),
        source: e,
    })?;

    let mut writer = fs::File::create(dest).map_err(|e| CopyError::FileCopy {
        src: src.to_path_buf(),
        dst: dest.to_path_buf(),
        source: e,
    })?;

    let mut buf = vec![0u8; buffer_size];
    loop {
        let n = reader.read(&mut buf).map_err(|e| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).unwrap_or(&buf);
        writer.write_all(chunk).map_err(|e| CopyError::FileCopy {
            src: src.to_path_buf(),
            dst: dest.to_path_buf(),
            source: e,
        })?;
    }

    Ok(())
}

/// Batch-copy small files using a tar stream (pack → unpack in memory).
fn copy_small_files_tar(
    files: &[(usize, &Path, &Path)],
    source_root: &Path,
    result: &mut CopyResult,
    progress: &dyn ProgressPort,
    files_total: u64,
    bytes_total: u64,
    plan: &CopyPlan,
) {
    // Pack all small files into a tar archive in memory.
    let mut archive_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive_buf);
        for &(_idx, src, _dest) in files {
            let relative = src.strip_prefix(source_root).unwrap_or(src);
            if let Err(e) = builder.append_path_with_name(src, relative) {
                let err = CopyError::TarPack {
                    path: src.to_path_buf(),
                    source: e,
                };
                warn!("{err}");
                result.errors.push(err);
            }
        }
        if let Err(e) = builder.finish() {
            warn!("tar finish error: {e}");
        }
    }

    // Unpack the tar archive to the destination.
    let mut archive = tar::Archive::new(archive_buf.as_slice());
    match archive.unpack(&plan.dest_root) {
        Ok(()) => {
            for &(idx, _src, _dest) in files {
                let entry_size = plan.entries.get(idx).map_or(0, |e| e.size);
                result.files_copied += 1;
                result.bytes_copied += entry_size;
            }

            progress.update(&ProgressUpdate::CopyProgress {
                files_copied: result.files_copied + result.files_linked,
                files_total,
                bytes_copied: result.bytes_copied,
                bytes_total,
            });
        }
        Err(e) => {
            let err = CopyError::TarUnpack {
                path: plan.dest_root.clone(),
                source: e,
            };
            warn!("{err}");
            result.errors.push(err);
        }
    }

    // Preserve permissions for tar-copied files.
    for &(idx, _src, dest) in files {
        if let Some(entry) = plan.entries.get(idx) {
            preserve_permissions(&entry.path, Path::new(dest), entry.permissions);
        }
    }
}

/// Preserve file permissions from source on destination.
#[cfg(unix)]
fn preserve_permissions(src: &Path, dest: &Path, permissions: u32) {
    use std::os::unix::fs::PermissionsExt;

    if permissions != 0 {
        let perms = fs::Permissions::from_mode(permissions);
        if let Err(e) = fs::set_permissions(dest, perms) {
            debug!("could not set permissions on {}: {e}", dest.display());
        }
    } else if let Ok(src_meta) = fs::metadata(src)
        && let Err(e) = fs::set_permissions(dest, src_meta.permissions())
    {
        debug!("could not copy permissions to {}: {e}", dest.display());
    }
}

#[cfg(not(unix))]
fn preserve_permissions(_src: &Path, _dest: &Path, _permissions: u32) {
    // No-op on non-Unix platforms.
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::domain::{CopyPlan, DedupGroup, FileEntry};
    use crate::ports::NullProgress;

    fn make_plan(
        src: &Path,
        dest: &Path,
        entries: Vec<FileEntry>,
        dedup_groups: Vec<DedupGroup>,
    ) -> CopyPlan {
        CopyPlan {
            source_root: src.to_path_buf(),
            dest_root: dest.to_path_buf(),
            entries,
            dedup_groups,
        }
    }

    #[test]
    fn copy_simple_directory_tree() {
        let src_dir = TempDir::new().expect("src tempdir");
        let dest_dir = TempDir::new().expect("dest tempdir");
        let src = src_dir.path();
        let dest = dest_dir.path();

        // Create source structure.
        fs::create_dir_all(src.join("subdir")).expect("mkdir");
        fs::write(src.join("a.txt"), "hello").expect("write");
        fs::write(src.join("subdir/b.txt"), "world").expect("write");

        let entries = vec![
            FileEntry::new(src.join("a.txt"), 5),
            FileEntry::new(src.join("subdir/b.txt"), 5),
        ];
        let plan = make_plan(src, dest, entries, vec![]);
        let config = CopyConfig {
            buffer_size: 4096,
            small_file_threshold: 0, // Force large-file path.
            ..CopyConfig::default()
        };

        let result = execute_copy_plan(&plan, &config, &NullProgress).expect("copy");

        assert_eq!(result.files_copied, 2);
        assert_eq!(result.errors.len(), 0);

        // Verify contents.
        assert_eq!(
            fs::read_to_string(dest.join("a.txt")).expect("read"),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(dest.join("subdir/b.txt")).expect("read"),
            "world"
        );
    }

    #[test]
    fn copy_with_hard_links_for_duplicates() {
        let src_dir = TempDir::new().expect("src tempdir");
        let dest_dir = TempDir::new().expect("dest tempdir");
        let src = src_dir.path();
        let dest = dest_dir.path();

        let content = "duplicate content";
        fs::write(src.join("canonical.txt"), content).expect("write");
        fs::write(src.join("dupe.txt"), content).expect("write");

        let entries = vec![
            FileEntry::new(src.join("canonical.txt"), content.len() as u64),
            FileEntry::new(src.join("dupe.txt"), content.len() as u64),
        ];
        let dedup_groups = vec![DedupGroup {
            canonical: 0,
            duplicates: vec![1],
        }];
        let plan = make_plan(src, dest, entries, dedup_groups);
        let config = CopyConfig {
            buffer_size: 4096,
            small_file_threshold: 0,
            ..CopyConfig::default()
        };

        let result = execute_copy_plan(&plan, &config, &NullProgress).expect("copy");

        assert_eq!(result.files_copied, 1);
        assert_eq!(result.files_linked, 1);

        // Both files exist with same content.
        assert_eq!(
            fs::read_to_string(dest.join("canonical.txt")).expect("read"),
            content
        );
        assert_eq!(
            fs::read_to_string(dest.join("dupe.txt")).expect("read"),
            content
        );

        // Verify they are hard-linked (same inode).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m1 = fs::metadata(dest.join("canonical.txt")).expect("meta");
            let m2 = fs::metadata(dest.join("dupe.txt")).expect("meta");
            assert_eq!(m1.ino(), m2.ino());
        }
    }

    #[test]
    fn copy_small_files_via_tar() {
        let src_dir = TempDir::new().expect("src tempdir");
        let dest_dir = TempDir::new().expect("dest tempdir");
        let src = src_dir.path();
        let dest = dest_dir.path();

        fs::write(src.join("small1.txt"), "tiny").expect("write");
        fs::write(src.join("small2.txt"), "also tiny").expect("write");

        let entries = vec![
            FileEntry::new(src.join("small1.txt"), 4),
            FileEntry::new(src.join("small2.txt"), 9),
        ];
        let plan = make_plan(src, dest, entries, vec![]);
        let config = CopyConfig {
            buffer_size: 4096,
            small_file_threshold: u64::MAX, // Everything is "small".
            ..CopyConfig::default()
        };

        let result = execute_copy_plan(&plan, &config, &NullProgress).expect("copy");

        assert_eq!(result.files_copied, 2);
        assert_eq!(
            fs::read_to_string(dest.join("small1.txt")).expect("read"),
            "tiny"
        );
        assert_eq!(
            fs::read_to_string(dest.join("small2.txt")).expect("read"),
            "also tiny"
        );
    }

    #[test]
    fn map_source_to_dest_works() {
        let src_root = Path::new("/src/root");
        let dest_root = Path::new("/dest/root");
        let src_path = Path::new("/src/root/subdir/file.txt");

        let result = map_source_to_dest(src_path, src_root, dest_root);
        assert_eq!(result, PathBuf::from("/dest/root/subdir/file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn copy_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = TempDir::new().expect("src tempdir");
        let dest_dir = TempDir::new().expect("dest tempdir");
        let src = src_dir.path();
        let dest = dest_dir.path();

        let file_path = src.join("exec.sh");
        fs::write(&file_path, "#!/bin/sh\necho hi").expect("write");
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut entry = FileEntry::new(file_path, 18);
        entry.permissions = 0o755;

        let plan = make_plan(src, dest, vec![entry], vec![]);
        let config = CopyConfig {
            buffer_size: 4096,
            small_file_threshold: 0,
            ..CopyConfig::default()
        };

        execute_copy_plan(&plan, &config, &NullProgress).expect("copy");

        let dest_meta = fs::metadata(dest.join("exec.sh")).expect("metadata");
        let mode = dest_meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn empty_plan_succeeds() {
        let src_dir = TempDir::new().expect("src tempdir");
        let dest_dir = TempDir::new().expect("dest tempdir");

        let plan = make_plan(src_dir.path(), dest_dir.path(), vec![], vec![]);
        let config = CopyConfig::default();

        let result = execute_copy_plan(&plan, &config, &NullProgress).expect("copy");
        assert_eq!(result.files_copied, 0);
        assert_eq!(result.files_linked, 0);
        assert_eq!(result.bytes_copied, 0);
    }
}
