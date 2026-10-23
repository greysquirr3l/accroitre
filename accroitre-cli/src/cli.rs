//! Command line surface of `accro`.
//!
//! The argument structs and the location parser live here so that both the
//! binary and the integration tests can reach them.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "accro", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Copy files from source to destination (default when no subcommand given).
    Copy(Box<CopyArgs>),

    /// Hash files and output JSON for remote dedup.
    Hash(HashArgs),

    /// Check for or install updates from GitHub releases.
    Update(UpdateArgs),

    /// Show version information including git SHA.
    Version,
}

/// Arguments for the copy (and default) operation.
#[derive(Parser, Debug)]
// CLI arg structs legitimately collect many boolean flags; refactoring to a
// separate flags struct would hurt clap's derive ergonomics without benefit.
#[allow(clippy::struct_excessive_bools)]
pub struct CopyArgs {
    /// Source path.  Use `user@host:/path` for remote SSH sources.
    #[arg(value_name = "SOURCE")]
    pub source: String,

    /// Destination path.  Use `user@host:/path` for remote SSH destinations.
    #[arg(value_name = "DESTINATION")]
    pub destination: String,

    // ── Transfer tuning ───────────────────────────────────────────────
    /// Read/write buffer size in megabytes.
    #[arg(long, default_value_t = 64, value_name = "MB")]
    pub buffer: u32,

    /// Number of worker threads.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    // ── Behaviour flags ───────────────────────────────────────────────
    /// Show what would be copied without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip post-copy verification.
    #[arg(long)]
    pub no_verify: bool,

    /// Disable content deduplication (hard-linking).
    #[arg(long)]
    pub no_dedup: bool,

    /// Strategy for resolving duplicate files during the dedup phase.
    ///
    /// `hardlink` (default): share an inode — zero extra space, divergence impossible.
    /// `clone`: copy-on-write where the filesystem supports it (`APFS`, `btrfs`, `XFS`, `ReFS`);
    /// duplicates start identical but can diverge independently.
    #[arg(long, default_value = "hardlink", value_parser = ["hardlink", "clone"], value_name = "STRATEGY")]
    pub link_strategy: String,

    /// Disable the on-disk hash cache.
    #[arg(long)]
    pub no_cache: bool,

    /// Overwrite existing files even when they appear up-to-date.
    #[arg(long)]
    pub force: bool,

    /// Overwrite existing files that differ in size or modification time.
    #[arg(long)]
    pub overwrite: bool,

    /// Delete destination files not present in source (sync mode).
    #[arg(long)]
    pub delete: bool,

    /// Exclude paths matching the given glob (repeatable).
    #[arg(long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Write a JSON log to the given file.
    #[arg(long, value_name = "FILE")]
    pub log_file: Option<PathBuf>,

    /// Suppress TUI; only show errors and the final summary.
    #[arg(long, short)]
    pub quiet: bool,

    // ── SSH source flags ──────────────────────────────────────────────
    /// SSH port for the source host.
    #[arg(long, value_name = "PORT", default_value_t = 22)]
    pub ssh_src_port: u16,

    /// Path to the SSH private key for the source host.
    #[arg(long, value_name = "FILE")]
    pub ssh_src_key: Option<PathBuf>,

    /// Password for the source host (prefer key-based auth).
    #[arg(long, value_name = "PASS")]
    pub ssh_src_password: Option<String>,

    // ── SSH destination flags ─────────────────────────────────────────
    /// SSH port for the destination host.
    #[arg(long, value_name = "PORT", default_value_t = 22)]
    pub ssh_dst_port: u16,

    /// Path to the SSH private key for the destination host.
    #[arg(long, value_name = "FILE")]
    pub ssh_dst_key: Option<PathBuf>,

    /// Password for the destination host (prefer key-based auth).
    #[arg(long, value_name = "PASS")]
    pub ssh_dst_password: Option<String>,

    // ── SSH connection flags ──────────────────────────────────────────
    /// Authenticate with the SSH agent instead of a key file.
    #[arg(long)]
    pub ssh_agent: bool,

    /// Path to the `known_hosts` file used to verify server host keys.
    #[arg(long, value_name = "FILE")]
    pub ssh_known_hosts: Option<PathBuf>,

    /// Reject SSH servers whose host key is not yet in `known_hosts`.
    #[arg(long)]
    pub ssh_strict_host_key: bool,

    // ── Compression ───────────────────────────────────────────────────
    /// Compress data in transit (gzip).
    #[arg(long, short = 'z')]
    pub compress: bool,
}

/// Arguments for the hash subcommand.
#[derive(Parser, Debug)]
pub struct HashArgs {
    /// Paths to hash.  Directories are walked recursively.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Hash algorithm to use.
    #[arg(long, default_value = "xxhash128", value_parser = ["xxhash128", "blake3"])]
    pub algorithm: String,

    /// Number of worker threads.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Emit one JSON object per line: `{"path": "...", "hash": "..."}`.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for the update subcommand.
#[derive(Parser, Debug)]
pub struct UpdateArgs {
    /// Only check for updates; do not install.
    #[arg(long)]
    pub check: bool,

    /// Download and verify the target release without replacing the binary.
    /// Useful for CI smoke tests of the GitHub release + SHA-256 pipeline.
    #[arg(long)]
    pub dry_run: bool,

    /// Install a specific version (e.g. `1.2.0`).  Defaults to latest.
    #[arg(long)]
    pub version: Option<String>,
}

// ── SSH path parsing ──────────────────────────────────────────────────────────

/// A parsed path that is either local or refers to a remote SSH host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local filesystem path.
    Local(PathBuf),
    /// A remote path accessed via SSH.
    Remote {
        user: String,
        host: String,
        path: PathBuf,
    },
}

/// Parse a source/destination string into a [`Location`].
///
/// Remote paths follow the `user@host:/path` convention.  To avoid treating
/// Windows drive letters (e.g. `C:\`) as SSH paths the colon must be followed
/// by `/` or `\` when the part before the colon contains no `@`.
#[must_use]
pub fn parse_location(input: &str) -> Location {
    // Look for user@host:/path — the `@` must precede the `:`.
    if let Some(at_pos) = input.find('@') {
        let after_at = &input[at_pos + 1..];

        // Handle bracketed IPv6: user@[::1]:/path
        let colon_pos = if after_at.starts_with('[') {
            after_at.find(']').and_then(|bracket_end| {
                // The colon must immediately follow the closing bracket.
                if after_at.as_bytes().get(bracket_end + 1) == Some(&b':') {
                    Some(at_pos + 1 + bracket_end + 1)
                } else {
                    None
                }
            })
        } else {
            after_at.find(':').map(|p| at_pos + 1 + p)
        };

        if let Some(colon_pos) = colon_pos {
            let user = &input[..at_pos];
            let host = &input[at_pos + 1..colon_pos];
            let path = &input[colon_pos + 1..];

            if !user.is_empty() && !host.is_empty() && !path.is_empty() {
                return Location::Remote {
                    user: user.to_owned(),
                    host: host.to_owned(),
                    path: PathBuf::from(path),
                };
            }
        }
    }
    Location::Local(PathBuf::from(input))
}

// The CLI argument structs are part of the public surface of the crate.
pub use Cli as CliRoot;
