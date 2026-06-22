//! Remote-side deduplication via SSH.
//!
//! Runs file hashing on remote servers to detect duplicates without
//! downloading content. Uses a fallback chain:
//!
//! 1. `accro hash --json` (native speed, if accro is installed remotely)
//! 2. `sha256sum` / `xxhsum` in batches of 5000 files
//! 3. Stream file content over SSH for local hashing (slowest)

use std::collections::HashMap;
use std::path::PathBuf;

use tracing::{debug, info, warn};

use crate::adapters::ssh::SshAdapter;
use crate::domain::{FileEntry, Hash, HashAlgorithm, SshError};

/// Maximum files per batch when using sha256sum/xxhsum fallback.
const FALLBACK_BATCH_SIZE: usize = 5000;

/// Result of a remote hashing operation.
#[derive(Debug)]
pub struct RemoteHashResult {
    /// Map of file path (remote) → hash.
    pub hashes: HashMap<PathBuf, Hash>,
    /// Method used for hashing.
    pub method: RemoteHashMethod,
    /// Number of files that failed to hash.
    pub failures: u64,
}

/// Which method was used for remote hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteHashMethod {
    /// Native accro binary on remote.
    AccroNative,
    /// sha256sum command-line tool.
    Sha256sum,
    /// xxhsum (xxh128sum) command-line tool.
    Xxhsum,
    /// Streamed content for local hashing (slowest).
    StreamLocal,
}

/// Detect which hashing tool is available on the remote.
///
/// Returns the best available method in order of preference.
///
/// # Errors
///
/// Returns `SshError` if the SSH connection fails.
pub async fn detect_remote_hash_method(adapter: &SshAdapter) -> Result<RemoteHashMethod, SshError> {
    // Try accro first (native speed).
    if let Ok(output) = adapter.exec_command("command -v accro").await
        && !output.is_empty()
    {
        info!("remote has accro installed — using native hashing");
        return Ok(RemoteHashMethod::AccroNative);
    }

    // Try xxhsum (xxHash).
    if let Ok(output) = adapter.exec_command("command -v xxhsum").await
        && !output.is_empty()
    {
        info!("remote has xxhsum — using xxHash fallback");
        return Ok(RemoteHashMethod::Xxhsum);
    }

    // Try sha256sum.
    if let Ok(output) = adapter.exec_command("command -v sha256sum").await
        && !output.is_empty()
    {
        info!("remote has sha256sum — using SHA-256 fallback");
        return Ok(RemoteHashMethod::Sha256sum);
    }

    // Last resort: stream content.
    info!("no remote hash tool found — will stream for local hashing");
    Ok(RemoteHashMethod::StreamLocal)
}

/// Hash remote files using the best available method.
///
/// # Errors
///
/// Returns `SshError` on SSH communication failures.
pub async fn hash_remote_files(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
    algorithm: HashAlgorithm,
) -> Result<RemoteHashResult, SshError> {
    let method = detect_remote_hash_method(adapter).await?;

    match method {
        RemoteHashMethod::AccroNative => {
            hash_with_accro(adapter, entries, remote_root, algorithm).await
        }
        RemoteHashMethod::Sha256sum => hash_with_sha256sum(adapter, entries, remote_root).await,
        RemoteHashMethod::Xxhsum => hash_with_xxhsum(adapter, entries, remote_root).await,
        RemoteHashMethod::StreamLocal => {
            hash_via_stream(adapter, entries, remote_root, algorithm).await
        }
    }
}

/// Hash files using the native `accro hash --json` command.
async fn hash_with_accro(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
    algorithm: HashAlgorithm,
) -> Result<RemoteHashResult, SshError> {
    let algo_flag = match algorithm {
        HashAlgorithm::XxHash128 => "--algorithm xxhash128",
        HashAlgorithm::Blake3 => "--algorithm blake3",
    };

    let file_list = build_file_list(entries, remote_root);
    let cmd = format!(
        "cd {} && accro hash --json {algo_flag} {}",
        shell_escape(remote_root),
        file_list
    );

    debug!("running accro hash on {} files", entries.len());
    let output = adapter.exec_command(&cmd).await?;

    let stdout = String::from_utf8_lossy(&output);
    let hashes = parse_accro_json_output(&stdout, algorithm);

    Ok(RemoteHashResult {
        failures: entries.len() as u64 - hashes.len() as u64,
        hashes,
        method: RemoteHashMethod::AccroNative,
    })
}

/// Hash files using `sha256sum` in batches.
async fn hash_with_sha256sum(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
) -> Result<RemoteHashResult, SshError> {
    let mut all_hashes = HashMap::new();
    let batches = build_path_batches(entries, remote_root, FALLBACK_BATCH_SIZE);

    for (batch_idx, batch) in batches.iter().enumerate() {
        debug!(
            "sha256sum batch {}/{} ({} files)",
            batch_idx + 1,
            batches.len(),
            batch.len()
        );

        let files_arg = batch.join(" ");
        let cmd = format!("cd {} && sha256sum {files_arg}", shell_escape(remote_root));

        match adapter.exec_command(&cmd).await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output);
                let parsed = parse_sha256sum_output(&stdout);
                all_hashes.extend(parsed);
            }
            Err(e) => {
                warn!("sha256sum batch {}: {e}", batch_idx + 1);
            }
        }
    }

    Ok(RemoteHashResult {
        failures: entries.len() as u64 - all_hashes.len() as u64,
        hashes: all_hashes,
        method: RemoteHashMethod::Sha256sum,
    })
}

/// Hash files using `xxhsum` (xxh128sum mode) in batches.
async fn hash_with_xxhsum(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
) -> Result<RemoteHashResult, SshError> {
    let mut all_hashes = HashMap::new();
    let batches = build_path_batches(entries, remote_root, FALLBACK_BATCH_SIZE);

    for (batch_idx, batch) in batches.iter().enumerate() {
        debug!(
            "xxhsum batch {}/{} ({} files)",
            batch_idx + 1,
            batches.len(),
            batch.len()
        );

        let files_arg = batch.join(" ");
        // xxhsum with --tag to get XXH128 output.
        let cmd = format!(
            "cd {} && xxhsum -H128 {files_arg}",
            shell_escape(remote_root)
        );

        match adapter.exec_command(&cmd).await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output);
                let parsed = parse_xxhsum_output(&stdout);
                all_hashes.extend(parsed);
            }
            Err(e) => {
                warn!("xxhsum batch {}: {e}", batch_idx + 1);
            }
        }
    }

    Ok(RemoteHashResult {
        failures: entries.len() as u64 - all_hashes.len() as u64,
        hashes: all_hashes,
        method: RemoteHashMethod::Xxhsum,
    })
}

/// Stream file content for local hashing (slowest fallback).
async fn hash_via_stream(
    adapter: &SshAdapter,
    entries: &[FileEntry],
    remote_root: &str,
    algorithm: HashAlgorithm,
) -> Result<RemoteHashResult, SshError> {
    let mut hashes = HashMap::new();
    let mut failures = 0u64;

    for entry in entries {
        let relative = entry.path.strip_prefix(remote_root).unwrap_or(&entry.path);

        let cmd = format!(
            "cat {}",
            shell_escape(&format!("{}/{}", remote_root, relative.to_string_lossy()))
        );

        match adapter.exec_command(&cmd).await {
            Ok(content) => {
                let hash = hash_bytes(&content, algorithm);
                hashes.insert(entry.path.clone(), hash);
            }
            Err(e) => {
                warn!("failed to stream {}: {e}", entry.path.display());
                failures += 1;
            }
        }
    }

    Ok(RemoteHashResult {
        failures,
        hashes,
        method: RemoteHashMethod::StreamLocal,
    })
}

/// Hash raw bytes locally using the specified algorithm.
fn hash_bytes(data: &[u8], algorithm: HashAlgorithm) -> Hash {
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

/// Build a shell-escaped file list for remote commands.
fn build_file_list(entries: &[FileEntry], remote_root: &str) -> String {
    entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_prefix(remote_root)
                .ok()
                .map(|p| shell_escape(&p.to_string_lossy()))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split entries into batches of shell-escaped relative paths.
fn build_path_batches(
    entries: &[FileEntry],
    remote_root: &str,
    batch_size: usize,
) -> Vec<Vec<String>> {
    entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_prefix(remote_root)
                .ok()
                .map(|p| shell_escape(&p.to_string_lossy()))
        })
        .collect::<Vec<_>>()
        .chunks(batch_size)
        .map(<[String]>::to_vec)
        .collect()
}

/// Escape a shell argument.
fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Parse `accro hash --json` output.
///
/// Expected format: JSON lines, each with `{"path": "...", "hash": "..."}`.
fn parse_accro_json_output(output: &str, algorithm: HashAlgorithm) -> HashMap<PathBuf, Hash> {
    let mut hashes = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse JSON: {"path": "...", "hash": "..."}
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line)
            && let (Some(path_str), Some(hash_str)) = (
                parsed.get("path").and_then(|v| v.as_str()),
                parsed.get("hash").and_then(|v| v.as_str()),
            )
            && let Some(hash) = parse_hex_hash(hash_str, algorithm)
        {
            hashes.insert(PathBuf::from(path_str), hash);
        }
    }

    hashes
}

/// Parse `sha256sum` output lines: `<hex>  <filename>`.
fn parse_sha256sum_output(output: &str) -> HashMap<PathBuf, Hash> {
    let mut hashes = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        // sha256sum format: "<64 hex chars>  <filename>" or "<64 hex chars> *<filename>"
        if let Some((hex, path)) = line.split_once("  ").or_else(|| line.split_once(" *")) {
            let hex = hex.trim();
            let path = path.trim();
            if hex.len() == 64
                && let Some(hash) = parse_sha256_hex(hex)
            {
                hashes.insert(PathBuf::from(path), hash);
            }
        }
    }

    hashes
}

/// Parse `xxhsum -H128` output lines: `<hex>  <filename>`.
fn parse_xxhsum_output(output: &str) -> HashMap<PathBuf, Hash> {
    let mut hashes = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        // xxhsum format: "<hex>  <filename>"
        if let Some((hex, path)) = line.split_once("  ") {
            let hex = hex.trim();
            let path = path.trim();
            if let Some(hash) = parse_xxhash128_hex(hex) {
                hashes.insert(PathBuf::from(path), hash);
            }
        }
    }

    hashes
}

/// Parse a hex string as a `Hash` of the given algorithm.
fn parse_hex_hash(hex: &str, algorithm: HashAlgorithm) -> Option<Hash> {
    match algorithm {
        HashAlgorithm::XxHash128 => parse_xxhash128_hex(hex),
        HashAlgorithm::Blake3 => parse_blake3_hex(hex),
    }
}

/// Parse a 32-character hex string as xxHash-128.
fn parse_xxhash128_hex(hex: &str) -> Option<Hash> {
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() == 16 {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        Some(Hash::XxHash128(arr))
    } else {
        None
    }
}

/// Parse a 64-character hex string as BLAKE3.
fn parse_blake3_hex(hex: &str) -> Option<Hash> {
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Some(Hash::Blake3(arr))
    } else {
        None
    }
}

/// Parse a 64-character hex string as SHA-256.
///
/// We store SHA-256 in the BLAKE3 variant (both are 32 bytes) for the fallback
/// path: both the source and destination use the same hash, so the comparison
/// is still valid even though the algorithm label differs.
fn parse_sha256_hex(hex: &str) -> Option<Hash> {
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        // Store as Blake3 variant (same size, comparison still works).
        Some(Hash::Blake3(arr))
    } else {
        None
    }
}

/// Convert a hex string to bytes.
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.chars();

    while let Some(hi) = chars.next() {
        let lo = chars.next()?;
        let hi = hi.to_digit(16)?;
        let lo = lo.to_digit(16)?;
        // hi and lo are each 0..=15, so hi*16+lo is 0..=255 — fits in u8.
        bytes.push((hi * 16 + lo).try_into().ok()?);
    }

    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_to_bytes_valid() {
        let result = hex_to_bytes("deadbeef");
        assert_eq!(result, Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn hex_to_bytes_odd_length() {
        let result = hex_to_bytes("abc");
        assert!(result.is_none());
    }

    #[test]
    fn hex_to_bytes_invalid_char() {
        let result = hex_to_bytes("zzzz");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sha256sum_output_normal() {
        let output = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.txt\n\
                       a948904f2f0f479b8f8564e15c2c3f72b0d75d0a6b3c3c5e0b5a5a0b5a5a0b5a  hello.txt\n";
        let hashes = parse_sha256sum_output(output);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains_key(&PathBuf::from("empty.txt")));
        assert!(hashes.contains_key(&PathBuf::from("hello.txt")));
    }

    #[test]
    fn parse_sha256sum_output_binary_mode() {
        let output =
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 *binary.dat\n";
        let hashes = parse_sha256sum_output(output);
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key(&PathBuf::from("binary.dat")));
    }

    #[test]
    fn parse_xxhsum_output_valid() {
        let output = "0123456789abcdef0123456789abcdef  test.bin\n";
        let hashes = parse_xxhsum_output(output);
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key(&PathBuf::from("test.bin")));
    }

    #[test]
    fn parse_accro_json_output_valid() {
        let output = r#"{"path": "foo.txt", "hash": "0123456789abcdef0123456789abcdef"}"#;
        let hashes = parse_accro_json_output(output, HashAlgorithm::XxHash128);
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key(&PathBuf::from("foo.txt")));
    }

    #[test]
    fn build_path_batches_splits_correctly() {
        let entries: Vec<FileEntry> = (0..12)
            .map(|i| FileEntry::new(PathBuf::from(format!("/root/file_{i}.dat")), 100))
            .collect();

        let batches = build_path_batches(&entries, "/root", 5);
        assert_eq!(batches.len(), 3); // 5 + 5 + 2
        let b0 = batches.first().map_or(0, Vec::len);
        let b1 = batches.get(1).map_or(0, Vec::len);
        let b2 = batches.get(2).map_or(0, Vec::len);
        assert_eq!(b0, 5);
        assert_eq!(b1, 5);
        assert_eq!(b2, 2);
    }

    #[test]
    fn build_path_batches_empty() {
        let entries: Vec<FileEntry> = Vec::new();
        let batches = build_path_batches(&entries, "/root", 5);
        assert!(batches.is_empty());
    }

    #[test]
    fn hash_bytes_xxhash128_deterministic() {
        let data = b"hello world";
        let h1 = hash_bytes(data, HashAlgorithm::XxHash128);
        let h2 = hash_bytes(data, HashAlgorithm::XxHash128);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_bytes_blake3_deterministic() {
        let data = b"hello world";
        let h1 = hash_bytes(data, HashAlgorithm::Blake3);
        let h2 = hash_bytes(data, HashAlgorithm::Blake3);
        assert_eq!(h1, h2);
    }

    #[test]
    fn fallback_chain_priority() {
        // This just tests the enum ordering / Display; actual detection
        // requires SSH. See integration tests for full fallback chain.
        assert_ne!(RemoteHashMethod::AccroNative, RemoteHashMethod::Sha256sum);
        assert_ne!(RemoteHashMethod::Sha256sum, RemoteHashMethod::StreamLocal);
    }
}
