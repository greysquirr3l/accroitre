//! `accro hash` — hash files so a remote host can deduplicate against them.
//!
//! The `--json` output is the contract with
//! [`remote_dedup`](accroitre::adapters::remote_dedup): one JSON object per
//! line carrying the path exactly as it was given on the command line and the
//! hex-encoded hash of the file's contents.

use std::path::{Path, PathBuf};

use accroitre::domain::{Hash, HashAlgorithm, HashError};
use accroitre::engine::hash::hash_file;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;

/// Default read buffer size for hashing (64 KiB).
const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

/// One JSON record emitted by `accro hash --json`.
#[derive(Debug, Serialize)]
struct HashRecord<'a> {
    /// Path exactly as supplied on the command line.
    path: &'a str,
    /// Hex-encoded content hash.
    hash: &'a str,
}

/// Parse the `--algorithm` argument.
fn parse_algorithm(name: &str) -> Result<HashAlgorithm> {
    match name {
        "xxhash128" => Ok(HashAlgorithm::XxHash128),
        "blake3" => Ok(HashAlgorithm::Blake3),
        other => bail!("unknown hash algorithm: {other}"),
    }
}

/// Expand the command-line paths into the list of regular files to hash.
///
/// Directories are walked recursively; the reported path keeps the directory
/// prefix the user typed, so `accro hash /mnt/data` reports
/// `/mnt/data/sub/file.txt`.
fn collect_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for path in paths {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("cannot access {}", path.display()))?;

        if metadata.is_file() {
            files.push(path.clone());
            continue;
        }

        if metadata.is_dir() {
            for entry in walkdir::WalkDir::new(path).follow_links(false) {
                let entry = entry.with_context(|| format!("cannot walk {}", path.display()))?;
                if entry.file_type().is_file() {
                    files.push(entry.path().to_path_buf());
                }
            }
            continue;
        }

        bail!("not a regular file or directory: {}", path.display());
    }

    Ok(files)
}

/// Hash a single path, returning the hash and the path it belongs to.
fn hash_one(
    path: &Path,
    algorithm: HashAlgorithm,
    buffer_size: usize,
) -> (PathBuf, Result<Hash, HashError>) {
    let hash = hash_file(path, algorithm, buffer_size);
    (path.to_path_buf(), hash)
}

/// Run the `accro hash` subcommand.
///
/// # Errors
///
/// Returns an error when the paths cannot be read or no files were found.
pub fn run(args: &crate::cli::HashArgs) -> Result<()> {
    let algorithm = parse_algorithm(&args.algorithm)?;
    let files = collect_files(&args.paths)?;

    if files.is_empty() {
        bail!("no files to hash");
    }

    let buffer_size = DEFAULT_BUFFER_SIZE;

    let results: Vec<(PathBuf, Result<Hash, HashError>)> = match args.threads {
        Some(threads) if threads > 0 => {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .context("cannot start hashing thread pool")?;
            pool.install(|| {
                files
                    .par_iter()
                    .map(|path| hash_one(path, algorithm, buffer_size))
                    .collect()
            })
        }
        _ => files
            .par_iter()
            .map(|path| hash_one(path, algorithm, buffer_size))
            .collect(),
    };

    let mut failures = 0usize;
    for (path, result) in results {
        match result {
            Ok(hash) => {
                if args.json {
                    let path_str = path.to_string_lossy();
                    let record = HashRecord {
                        path: &path_str,
                        hash: &hash.to_hex(),
                    };
                    println!(
                        "{}",
                        serde_json::to_string(&record).context("cannot encode hash record")?
                    );
                } else {
                    println!("{}  {}", hash.to_hex(), path.display());
                }
            }
            Err(e) => {
                eprintln!("accro hash: {e}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{failures} file(s) could not be hashed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HashRecord, parse_algorithm};
    use accroitre::domain::HashAlgorithm;

    #[test]
    fn known_algorithms_parse() {
        assert_eq!(
            parse_algorithm("xxhash128").ok(),
            Some(HashAlgorithm::XxHash128)
        );
        assert_eq!(parse_algorithm("blake3").ok(), Some(HashAlgorithm::Blake3));
    }

    #[test]
    fn unknown_algorithm_is_rejected() {
        assert!(parse_algorithm("md5").is_err());
    }

    #[test]
    fn json_record_has_path_and_hash_fields() {
        let record = HashRecord {
            path: "/mnt/data/a.txt",
            hash: "0011",
        };
        let json = serde_json::to_string(&record).ok();
        assert_eq!(
            json.as_deref(),
            Some(r#"{"path":"/mnt/data/a.txt","hash":"0011"}"#)
        );
    }

    #[test]
    fn json_record_escapes_special_characters() {
        let record = HashRecord {
            path: "/mnt/\"quoted\"/a.txt",
            hash: "00",
        };
        let json = serde_json::to_string(&record).ok();
        assert_eq!(
            json.as_deref(),
            Some(r#"{"path":"/mnt/\"quoted\"/a.txt","hash":"00"}"#)
        );
    }
}
