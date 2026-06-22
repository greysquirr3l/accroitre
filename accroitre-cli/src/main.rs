//! `accro` — CLI for accroître, a high-speed file copier.

pub mod pipeline;
pub mod tui;
pub mod update;

use std::path::PathBuf;

use accroitre::ports::ProgressPort;
use clap::{Parser, Subcommand};

/// Accroître — high-speed file copier with deduplication and SSH streaming.
///
/// Copy files at maximum speed with automatic deduplication, physical-order
/// reads, batched tar streaming, and transparent SSH transfers.
#[derive(Parser, Debug)]
#[command(name = "accro", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Copy files from source to destination (default when no subcommand given).
    Copy(CopyArgs),

    /// Hash files and output JSON for remote dedup.
    Hash(HashArgs),

    /// Check for or install updates from GitHub releases.
    Update(UpdateArgs),

    /// Show version information including git SHA.
    Version,
}

/// Arguments for the copy (and default) operation.
#[derive(Parser, Debug)]
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

    // ── Compression ───────────────────────────────────────────────────
    /// Compress data in transit (gzip).
    #[arg(long, short = 'z')]
    pub compress: bool,
}

/// Arguments for the hash subcommand.
#[derive(Parser, Debug)]
pub struct HashArgs {
    /// Paths to hash.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Hash algorithm to use.
    #[arg(long, default_value = "xxhash128", value_parser = ["xxhash128", "blake3"])]
    pub algorithm: String,

    /// Number of worker threads.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,
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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Copy(args)) => {
            let exit_code = run_copy(&args);
            std::process::exit(exit_code);
        }
        Some(Commands::Hash(_args)) => {
            // TODO(T17): wire up hash subcommand
            eprintln!("Hash command not yet implemented.");
        }
        Some(Commands::Update(args)) => {
            let exit_code = run_update(&args);
            std::process::exit(exit_code);
        }
        Some(Commands::Version) => {
            let version = env!("CARGO_PKG_VERSION");
            let sha = env!("ACCRO_GIT_SHA");
            println!("accro {version} ({sha})");
        }
        None => {
            // Default: show help when no subcommand or positional args given.
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
        }
    }
}

fn run_copy(args: &CopyArgs) -> i32 {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let src_loc = parse_location(&args.source);
    let dst_loc = parse_location(&args.destination);

    // Only local-to-local is wired in this task.
    if !matches!(
        (&src_loc, &dst_loc),
        (Location::Local(_), Location::Local(_))
    ) {
        eprintln!("Remote transfers are not yet implemented.");
        return pipeline::EXIT_FAILURE;
    }

    let quiet = args.quiet;
    let tui = tui::TuiProgress::new(quiet);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_for_handler = Arc::clone(&cancelled);

    // Install Ctrl-C handler.
    ctrlc_handler(cancelled_for_handler);

    let thread_count = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(thread_count)
        .enable_all()
        .build();

    let rt = match rt {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to start runtime: {e}");
            return pipeline::EXIT_FAILURE;
        }
    };

    let result = rt.block_on(pipeline::run_local_pipeline(
        args,
        &tui,
        Arc::clone(&cancelled),
    ));

    // Always finish the TUI (prints summary).
    tui.finish();

    match result {
        Ok(pipeline_result) => {
            if pipeline_result.cancelled {
                eprintln!("Operation cancelled by user.");
            }
            pipeline_result.exit_code()
        }
        Err(e) => {
            eprintln!("Fatal error: {e:#}");
            pipeline::EXIT_FAILURE
        }
    }
}

fn ctrlc_handler(cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Ok(rt) = rt {
            let _ = rt.block_on(async { tokio::signal::ctrl_c().await });
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("\nInterrupted — finishing current operation…");
        }
    });
}

fn run_update(args: &UpdateArgs) -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();

    let rt = match rt {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to start runtime: {e}");
            return pipeline::EXIT_FAILURE;
        }
    };

    let result = rt.block_on(update::run(args));
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Update failed: {e:#}");
            pipeline::EXIT_FAILURE
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_local_path() {
        assert_eq!(
            parse_location("/tmp/backup"),
            Location::Local(PathBuf::from("/tmp/backup"))
        );
    }

    #[test]
    fn parse_remote_ssh_path() {
        assert_eq!(
            parse_location("nick@server.example.com:/mnt/data"),
            Location::Remote {
                user: "nick".to_owned(),
                host: "server.example.com".to_owned(),
                path: PathBuf::from("/mnt/data"),
            }
        );
    }

    #[test]
    fn parse_remote_relative_path() {
        assert_eq!(
            parse_location("deploy@10.0.0.1:backups/daily"),
            Location::Remote {
                user: "deploy".to_owned(),
                host: "10.0.0.1".to_owned(),
                path: PathBuf::from("backups/daily"),
            }
        );
    }

    #[test]
    fn parse_ipv6_remote() {
        assert_eq!(
            parse_location("root@[::1]:/data"),
            Location::Remote {
                user: "root".to_owned(),
                host: "[::1]".to_owned(),
                path: PathBuf::from("/data"),
            }
        );
    }

    #[test]
    fn windows_drive_letter_stays_local() {
        // No `@` present, so this must not be mistaken for SSH.
        assert_eq!(
            parse_location("C:\\Users\\nick"),
            Location::Local(PathBuf::from("C:\\Users\\nick"))
        );
    }

    #[test]
    fn copy_args_defaults() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from(["accro", "copy", "/src", "/dst"])?;
        match cli.command {
            Some(Commands::Copy(args)) => {
                assert_eq!(args.source, "/src");
                assert_eq!(args.destination, "/dst");
                assert_eq!(args.buffer, 64);
                assert!(args.threads.is_none());
                assert!(!args.dry_run);
                assert!(!args.no_verify);
                assert!(!args.no_dedup);
                assert!(!args.no_cache);
                assert!(!args.force);
                assert!(!args.overwrite);
                assert!(!args.delete);
                assert!(args.exclude.is_empty());
                assert!(args.log_file.is_none());
                assert!(!args.quiet);
                assert_eq!(args.ssh_src_port, 22);
                assert_eq!(args.ssh_dst_port, 22);
                assert!(!args.compress);
            }
            _ => return Err("Expected Copy command".into()),
        }
        Ok(())
    }

    #[test]
    fn copy_args_all_flags() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from([
            "accro",
            "copy",
            "--buffer",
            "128",
            "--threads",
            "8",
            "--dry-run",
            "--no-verify",
            "--no-dedup",
            "--no-cache",
            "--force",
            "--overwrite",
            "--delete",
            "--exclude",
            "*.log",
            "--exclude",
            "tmp/**",
            "--log-file",
            "run.log",
            "--quiet",
            "--ssh-src-port",
            "2222",
            "--ssh-src-key",
            "/home/nick/.ssh/id_ed25519",
            "--ssh-src-password",
            "secret",
            "--ssh-dst-port",
            "2223",
            "--ssh-dst-key",
            "/keys/dst",
            "--ssh-dst-password",
            "other",
            "-z",
            "user@remote:/src",
            "admin@backup:/dst",
        ])?;

        match cli.command {
            Some(Commands::Copy(args)) => {
                assert_eq!(args.buffer, 128);
                assert_eq!(args.threads, Some(8));
                assert!(args.dry_run);
                assert!(args.no_verify);
                assert!(args.no_dedup);
                assert!(args.no_cache);
                assert!(args.force);
                assert!(args.overwrite);
                assert!(args.delete);
                assert_eq!(args.exclude, vec!["*.log", "tmp/**"]);
                assert_eq!(args.log_file, Some(PathBuf::from("run.log")));
                assert!(args.quiet);
                assert_eq!(args.ssh_src_port, 2222);
                assert_eq!(
                    args.ssh_src_key,
                    Some(PathBuf::from("/home/nick/.ssh/id_ed25519"))
                );
                assert_eq!(args.ssh_src_password, Some("secret".to_owned()));
                assert_eq!(args.ssh_dst_port, 2223);
                assert_eq!(args.ssh_dst_key, Some(PathBuf::from("/keys/dst")));
                assert_eq!(args.ssh_dst_password, Some("other".to_owned()));
                assert!(args.compress);
                assert_eq!(args.source, "user@remote:/src");
                assert_eq!(args.destination, "admin@backup:/dst");
            }
            _ => return Err("Expected Copy command".into()),
        }
        Ok(())
    }

    #[test]
    fn hash_subcommand_defaults() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from(["accro", "hash", "/data/file.bin"])?;
        match cli.command {
            Some(Commands::Hash(args)) => {
                assert_eq!(args.paths, vec![PathBuf::from("/data/file.bin")]);
                assert_eq!(args.algorithm, "xxhash128");
                assert!(args.threads.is_none());
            }
            _ => return Err("Expected Hash command".into()),
        }
        Ok(())
    }

    #[test]
    fn hash_subcommand_blake3() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from(["accro", "hash", "--algorithm", "blake3", "/a", "/b"])?;
        match cli.command {
            Some(Commands::Hash(args)) => {
                assert_eq!(args.algorithm, "blake3");
                assert_eq!(args.paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
            }
            _ => return Err("Expected Hash command".into()),
        }
        Ok(())
    }

    #[test]
    fn version_subcommand() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from(["accro", "version"])?;
        assert!(matches!(cli.command, Some(Commands::Version)));
        Ok(())
    }

    #[test]
    fn exclude_accepts_multiple_values() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from([
            "accro",
            "copy",
            "--exclude",
            "*.tmp",
            "--exclude",
            "*.bak",
            "--exclude",
            "node_modules/**",
            "/src",
            "/dst",
        ])?;
        match cli.command {
            Some(Commands::Copy(args)) => {
                assert_eq!(args.exclude.len(), 3);
                let e0 = args.exclude.first().ok_or("missing exclude 0")?;
                let e1 = args.exclude.get(1).ok_or("missing exclude 1")?;
                let e2 = args.exclude.get(2).ok_or("missing exclude 2")?;
                assert_eq!(e0, "*.tmp");
                assert_eq!(e1, "*.bak");
                assert_eq!(e2, "node_modules/**");
            }
            _ => return Err("Expected Copy command".into()),
        }
        Ok(())
    }

    #[test]
    fn update_dry_run_flag_parses() -> Result<(), Box<dyn std::error::Error>> {
        let cli = Cli::try_parse_from(["accro", "update", "--dry-run"])?;
        match cli.command {
            Some(Commands::Update(args)) => {
                assert!(args.dry_run);
                assert!(!args.check);
            }
            _ => return Err("Expected Update command".into()),
        }
        Ok(())
    }

    #[test]
    fn update_check_and_dry_run_are_independent() -> Result<(), Box<dyn std::error::Error>> {
        // `--check` and `--dry-run` should not be combinable in practice but
        // parsing-wise they're separate bools. Verify both can be set
        // independently without panics.
        let cli = Cli::try_parse_from(["accro", "update", "--check"])?;
        match cli.command {
            Some(Commands::Update(args)) => {
                assert!(args.check);
                assert!(!args.dry_run);
            }
            _ => return Err("Expected Update command".into()),
        }
        Ok(())
    }
}
