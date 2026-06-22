//! Chunked tar streaming over SSH.
//!
//! Provides local→remote, remote→local, and remote→remote (relay) tar pipe
//! transfers. All operations use raw SSH exec channels with tar — no SFTP
//! dependency. Transfers are split into configurable-size batches (~100 MB)
//! for progress reporting and partial failure recovery.

use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::adapters::ssh::SshAdapter;
use crate::domain::{CopyError, FileEntry};

/// Default batch size target for chunked tar streaming (~100 MB).
const DEFAULT_BATCH_SIZE: u64 = 100 * 1024 * 1024;

/// Result of a tar streaming operation.
#[derive(Debug)]
pub struct TarStreamResult {
    /// Total bytes transferred.
    pub bytes_transferred: u64,
    /// Number of files transferred.
    pub files_transferred: u64,
    /// Number of batches.
    pub batches_completed: u64,
    /// Errors encountered (non-fatal, per-batch).
    pub errors: Vec<CopyError>,
}

/// Configuration for tar streaming over SSH.
#[derive(Debug, Clone)]
pub struct TarStreamConfig {
    /// Target batch size in bytes (default: ~100 MB).
    pub batch_size: u64,
    /// Enable SSH compression (equivalent to `ssh -C`).
    pub compress: bool,
}

impl Default for TarStreamConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            compress: false,
        }
    }
}

/// A batch of files to transfer in a single tar stream.
struct Batch {
    /// Indices into the original entries slice.
    indices: Vec<usize>,
    /// Total size of files in this batch.
    total_size: u64,
}

/// Split file entries into batches targeting the configured size.
fn build_batches(entries: &[FileEntry], batch_size: u64) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut current = Batch {
        indices: Vec::new(),
        total_size: 0,
    };

    for (i, entry) in entries.iter().enumerate() {
        current.indices.push(i);
        current.total_size += entry.size;

        if current.total_size >= batch_size {
            batches.push(current);
            current = Batch {
                indices: Vec::new(),
                total_size: 0,
            };
        }
    }

    // Don't lose the last partial batch.
    if !current.indices.is_empty() {
        batches.push(current);
    }

    batches
}

/// Create a tar archive in memory from the given file entries.
fn create_tar_archive(entries: &[FileEntry], source_root: &Path) -> Result<Vec<u8>, CopyError> {
    let mut archive_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive_buf);
        for entry in entries {
            let relative = entry.path.strip_prefix(source_root).unwrap_or(&entry.path);
            builder
                .append_path_with_name(&entry.path, relative)
                .map_err(|e| CopyError::TarPack {
                    path: entry.path.clone(),
                    source: e,
                })?;
        }
        builder.finish().map_err(|e| CopyError::TarPack {
            path: source_root.to_path_buf(),
            source: e,
        })?;
    }
    Ok(archive_buf)
}

/// Extract a tar archive from memory into a destination directory.
fn extract_tar_archive(archive_data: &[u8], dest: &Path) -> Result<(), CopyError> {
    let mut archive = tar::Archive::new(archive_data);
    archive.unpack(dest).map_err(|e| CopyError::TarUnpack {
        path: dest.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Stream files from local to remote via tar pipes over SSH.
///
/// For each batch:
/// 1. Pack entries into an in-memory tar archive
/// 2. Send via `ssh exec "tar xf - -C <dest>"`
/// 3. Record progress
///
/// # Errors
///
/// Returns per-batch errors in `TarStreamResult.errors`. Returns a hard
/// `CopyError` only if no batches succeed.
pub async fn upload_tar_stream(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    source_root: &Path,
    remote_dest: &str,
    config: &TarStreamConfig,
) -> Result<TarStreamResult, CopyError> {
    let batches = build_batches(entries, config.batch_size);
    let batch_count = batches.len();
    info!(
        "uploading {} files in {batch_count} batches to {remote_dest}",
        entries.len()
    );

    let mut result = TarStreamResult {
        bytes_transferred: 0,
        files_transferred: 0,
        batches_completed: 0,
        errors: Vec::new(),
    };

    // Ensure remote destination exists.
    let mkdir_cmd = format!("mkdir -p {remote_dest}");
    if let Err(e) = adapter.exec_command(&mkdir_cmd).await {
        warn!("failed to create remote directory: {e}");
        return Err(CopyError::Transport {
            path: PathBuf::from(remote_dest),
            source: std::io::Error::other(format!("mkdir failed: {e}")),
        });
    }

    let extract_cmd = if config.compress {
        format!("gzip -d | tar xf - -C {remote_dest}")
    } else {
        format!("tar xf - -C {remote_dest}")
    };

    for (batch_idx, batch) in batches.iter().enumerate() {
        debug!(
            "uploading batch {}/{batch_count} ({} files, {} bytes)",
            batch_idx + 1,
            batch.indices.len(),
            batch.total_size
        );

        // Collect batch entries.
        let batch_entries: Vec<&FileEntry> = batch
            .indices
            .iter()
            .filter_map(|&i| entries.get(i))
            .collect();

        // Create the tar archive.
        let batch_file_entries: Vec<FileEntry> =
            batch_entries.iter().map(|e| (*e).clone()).collect();
        let archive_data = match create_tar_archive(&batch_file_entries, source_root) {
            Ok(data) => data,
            Err(e) => {
                warn!("batch {}: tar creation failed: {e}", batch_idx + 1);
                result.errors.push(e);
                continue;
            }
        };

        // Optionally compress.
        let send_data = if config.compress {
            compress_gzip(&archive_data).map_err(|e| CopyError::Transport {
                path: source_root.to_path_buf(),
                source: e,
            })?
        } else {
            archive_data
        };

        // Send tar data over SSH.
        match adapter.exec_with_stdin(&extract_cmd, &send_data).await {
            Ok(_) => {
                result.bytes_transferred += batch.total_size;
                result.files_transferred += batch.indices.len() as u64;
                result.batches_completed += 1;
                debug!(
                    "batch {}/{batch_count} complete ({} bytes)",
                    batch_idx + 1,
                    batch.total_size
                );
            }
            Err(e) => {
                warn!("batch {}: SSH upload failed: {e}", batch_idx + 1);
                result.errors.push(CopyError::Transport {
                    path: PathBuf::from(remote_dest),
                    source: std::io::Error::other(format!("SSH upload failed: {e}")),
                });
            }
        }
    }

    info!(
        "upload complete: {} files, {} bytes in {} batches ({} errors)",
        result.files_transferred,
        result.bytes_transferred,
        result.batches_completed,
        result.errors.len()
    );

    Ok(result)
}

/// Stream files from remote to local via tar pipes over SSH.
///
/// For each batch:
/// 1. Execute `tar cf -` on the remote for the batch's files
/// 2. Receive the tar stream
/// 3. Extract locally
///
/// # Errors
///
/// Returns per-batch errors in `TarStreamResult.errors`.
pub async fn download_tar_stream(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
    local_dest: &Path,
    config: &TarStreamConfig,
) -> Result<TarStreamResult, CopyError> {
    let batches = build_batches(entries, config.batch_size);
    let batch_count = batches.len();
    info!(
        "downloading {} files in {batch_count} batches from {remote_root}",
        entries.len()
    );

    let mut result = TarStreamResult {
        bytes_transferred: 0,
        files_transferred: 0,
        batches_completed: 0,
        errors: Vec::new(),
    };

    // Ensure local destination exists.
    std::fs::create_dir_all(local_dest).map_err(|e| CopyError::Transport {
        path: local_dest.to_path_buf(),
        source: e,
    })?;

    for (batch_idx, batch) in batches.iter().enumerate() {
        debug!(
            "downloading batch {}/{batch_count} ({} files, {} bytes)",
            batch_idx + 1,
            batch.indices.len(),
            batch.total_size
        );

        // Build the list of relative paths for remote tar.
        let file_list: Vec<String> = batch
            .indices
            .iter()
            .filter_map(|&i| entries.get(i))
            .filter_map(|e| {
                e.path
                    .strip_prefix(remote_root)
                    .ok()
                    .map(|p| shell_escape(p.to_string_lossy().as_ref()))
            })
            .collect();

        if file_list.is_empty() {
            continue;
        }

        let files_arg = file_list.join(" ");
        let tar_cmd = if config.compress {
            format!("tar cf - -C {remote_root} {files_arg} | gzip")
        } else {
            format!("tar cf - -C {remote_root} {files_arg}")
        };

        // Execute remote tar and collect output.
        match adapter.exec_command(&tar_cmd).await {
            Ok(tar_data) => {
                let extract_data = if config.compress {
                    match decompress_gzip(&tar_data) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!("batch {}: gzip decompression failed: {e}", batch_idx + 1);
                            result.errors.push(CopyError::Transport {
                                path: local_dest.to_path_buf(),
                                source: e,
                            });
                            continue;
                        }
                    }
                } else {
                    tar_data
                };

                match extract_tar_archive(&extract_data, local_dest) {
                    Ok(()) => {
                        result.bytes_transferred += batch.total_size;
                        result.files_transferred += batch.indices.len() as u64;
                        result.batches_completed += 1;
                        debug!(
                            "batch {}/{batch_count} complete ({} bytes)",
                            batch_idx + 1,
                            batch.total_size
                        );
                    }
                    Err(e) => {
                        warn!("batch {}: extraction failed: {e}", batch_idx + 1);
                        result.errors.push(e);
                    }
                }
            }
            Err(e) => {
                warn!("batch {}: SSH download failed: {e}", batch_idx + 1);
                result.errors.push(CopyError::Transport {
                    path: local_dest.to_path_buf(),
                    source: std::io::Error::other(format!("SSH download failed: {e}")),
                });
            }
        }
    }

    info!(
        "download complete: {} files, {} bytes in {} batches ({} errors)",
        result.files_transferred,
        result.bytes_transferred,
        result.batches_completed,
        result.errors.len()
    );

    Ok(result)
}

/// Relay files from one remote to another via local machine.
///
/// Source tar → SSH → local buffer → SSH → destination tar.
///
/// # Errors
///
/// Returns a hard `CopyError` if the relay fails.
pub async fn relay_tar_stream(
    source_adapter: &SshAdapter,
    dest_adapter: &SshAdapter,
    entries: &[FileEntry],
    source_root: &str,
    dest_root: &str,
    config: &TarStreamConfig,
) -> Result<TarStreamResult, CopyError> {
    let batches = build_batches(entries, config.batch_size);
    let batch_count = batches.len();
    info!(
        "relaying {} files in {batch_count} batches ({source_root} → {dest_root})",
        entries.len()
    );

    let mut result = TarStreamResult {
        bytes_transferred: 0,
        files_transferred: 0,
        batches_completed: 0,
        errors: Vec::new(),
    };

    // Ensure remote destination exists.
    let mkdir_cmd = format!("mkdir -p {dest_root}");
    if let Err(e) = dest_adapter.exec_command(&mkdir_cmd).await {
        warn!("failed to create remote directory: {e}");
        return Err(CopyError::Transport {
            path: PathBuf::from(dest_root),
            source: std::io::Error::other(format!("mkdir failed: {e}")),
        });
    }

    let extract_cmd = format!("tar xf - -C {dest_root}");

    for (batch_idx, batch) in batches.iter().enumerate() {
        debug!(
            "relaying batch {}/{batch_count} ({} files, {} bytes)",
            batch_idx + 1,
            batch.indices.len(),
            batch.total_size
        );

        // Build the list of relative paths for source tar.
        let file_list: Vec<String> = batch
            .indices
            .iter()
            .filter_map(|&i| entries.get(i))
            .filter_map(|e| {
                e.path
                    .strip_prefix(source_root)
                    .ok()
                    .map(|p| shell_escape(p.to_string_lossy().as_ref()))
            })
            .collect();

        if file_list.is_empty() {
            continue;
        }

        let files_arg = file_list.join(" ");
        let tar_cmd = format!("tar cf - -C {source_root} {files_arg}");

        // Download from source.
        let tar_data = match source_adapter.exec_command(&tar_cmd).await {
            Ok(data) => data,
            Err(e) => {
                warn!("batch {}: source download failed: {e}", batch_idx + 1);
                result.errors.push(CopyError::Transport {
                    path: PathBuf::from(source_root),
                    source: std::io::Error::other(format!("relay source failed: {e}")),
                });
                continue;
            }
        };

        // Upload to destination.
        match dest_adapter.exec_with_stdin(&extract_cmd, &tar_data).await {
            Ok(_) => {
                result.bytes_transferred += batch.total_size;
                result.files_transferred += batch.indices.len() as u64;
                result.batches_completed += 1;
                debug!(
                    "batch {}/{batch_count} complete ({} bytes)",
                    batch_idx + 1,
                    batch.total_size
                );
            }
            Err(e) => {
                warn!("batch {}: destination upload failed: {e}", batch_idx + 1);
                result.errors.push(CopyError::Transport {
                    path: PathBuf::from(dest_root),
                    source: std::io::Error::other(format!("relay dest failed: {e}")),
                });
            }
        }
    }

    info!(
        "relay complete: {} files, {} bytes in {} batches ({} errors)",
        result.files_transferred,
        result.bytes_transferred,
        result.batches_completed,
        result.errors.len()
    );

    Ok(result)
}

/// Escape a shell argument to prevent injection.
fn shell_escape(s: &str) -> String {
    // Wrap in single quotes, escaping any embedded single quotes.
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Compress data with gzip.
fn compress_gzip(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data)?;
    encoder.finish()
}

/// Decompress gzip data.
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_entries(
        dir: &Path,
        count: usize,
        size: u64,
    ) -> Result<Vec<FileEntry>, Box<dyn std::error::Error>> {
        let mut entries = Vec::new();
        for i in 0..count {
            let path = dir.join(format!("file_{i}.dat"));
            let byte = u8::try_from(i).map_err(|e| format!("count must be < 256: {e}"))?;
            let len = usize::try_from(size).map_err(|e| format!("size must fit in usize: {e}"))?;
            let data = vec![byte; len];
            std::fs::write(&path, &data)?;
            entries.push(FileEntry::new(path, size));
        }
        Ok(entries)
    }

    #[test]
    fn build_batches_single_batch() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let dir_path = dir.path();

        let entries = make_test_entries(dir_path, 5, 100)?;
        let batches = build_batches(&entries, 1000);
        assert_eq!(batches.len(), 1);
        let first = batches.first().ok_or("expected one batch")?;
        assert_eq!(first.indices.len(), 5);
        assert_eq!(first.total_size, 500);
        Ok(())
    }

    #[test]
    fn build_batches_multiple_batches() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let dir_path = dir.path();

        let entries = make_test_entries(dir_path, 10, 100)?;
        let batches = build_batches(&entries, 350);

        // 4 files × 100 = 400 >= 350 → batch 1
        // 4 files × 100 = 400 >= 350 → batch 2
        // 2 files × 100 = 200 < 350 → batch 3 (remainder)
        assert_eq!(batches.len(), 3);
        Ok(())
    }

    #[test]
    fn build_batches_empty_entries() {
        let entries: Vec<FileEntry> = Vec::new();
        let batches = build_batches(&entries, 1000);
        assert!(batches.is_empty());
    }

    #[test]
    fn tar_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let src_dir = TempDir::new()?;
        let src_path = src_dir.path();

        let entries = make_test_entries(src_path, 3, 256)?;

        let archive_data = create_tar_archive(&entries, src_path)?;

        let dest_dir = TempDir::new()?;
        let dest_path = dest_dir.path();

        extract_tar_archive(&archive_data, dest_path)?;

        // Verify files were extracted.
        for i in 0..3 {
            let extracted = dest_path.join(format!("file_{i}.dat"));
            assert!(extracted.exists(), "file_{i}.dat should exist");
            let bytes = std::fs::read(&extracted)?;
            assert_eq!(bytes.len(), 256);
            let expected = u8::try_from(i).map_err(|e| format!("loop index fits in u8: {e}"))?;
            assert!(bytes.iter().all(|&b| b == expected));
        }
        Ok(())
    }

    #[test]
    fn gzip_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let original = b"hello world, this is a test of gzip compression";
        let cdata = compress_gzip(original)?;
        let ddata = decompress_gzip(&cdata)?;
        assert_eq!(ddata.as_slice(), original);
        Ok(())
    }

    #[test]
    fn shell_escape_prevents_injection() {
        assert_eq!(shell_escape("simple"), "'simple'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
        assert_eq!(shell_escape("foo bar"), "'foo bar'");
        assert_eq!(shell_escape("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
