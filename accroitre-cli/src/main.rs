//! `accro` — CLI for accroître, a high-speed file copier.

use std::sync::Arc;

use accroitre::domain::TransferMode;
use accroitre::ports::ProgressPort;
use accroitre_cli::cli::{Cli, Commands, CopyArgs, Location, UpdateArgs, parse_location};
use accroitre_cli::{hash_cmd, pipeline, remote_pipeline, tui, update};
use clap::Parser;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Copy(args)) => {
            let exit_code = run_copy(&args);
            std::process::exit(exit_code);
        }
        Some(Commands::Hash(args)) => {
            let exit_code = match hash_cmd::run(&args) {
                Ok(()) => pipeline::EXIT_SUCCESS,
                Err(e) => {
                    eprintln!("accro hash: {e:#}");
                    pipeline::EXIT_FAILURE
                }
            };
            std::process::exit(exit_code);
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
    use std::sync::atomic::AtomicBool;

    let src_loc = parse_location(&args.source);
    let dst_loc = parse_location(&args.destination);

    let mode = transfer_mode(&src_loc, &dst_loc);

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

    let result = rt.block_on(dispatch_copy(
        args,
        mode,
        &src_loc,
        &dst_loc,
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

/// Determine the transfer mode implied by a source and destination location.
#[must_use]
pub const fn transfer_mode(source: &Location, destination: &Location) -> TransferMode {
    match (source, destination) {
        (Location::Local(_), Location::Local(_)) => TransferMode::LocalToLocal,
        (Location::Local(_), Location::Remote { .. }) => TransferMode::LocalToRemote,
        (Location::Remote { .. }, Location::Local(_)) => TransferMode::RemoteToLocal,
        (Location::Remote { .. }, Location::Remote { .. }) => TransferMode::RemoteToRemote,
    }
}

/// Run the pipeline that matches the transfer mode.
async fn dispatch_copy(
    args: &CopyArgs,
    mode: TransferMode,
    source: &Location,
    destination: &Location,
    tui: &tui::TuiProgress,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<pipeline::PipelineResult> {
    let known_hosts = remote_pipeline::KnownHostsOptions {
        path: args.ssh_known_hosts.clone(),
        strict: args.ssh_strict_host_key,
    };

    let source_endpoint = match source {
        Location::Remote { user, host, .. } => remote_pipeline::endpoint_for(
            user,
            host,
            args.ssh_src_port,
            args.ssh_src_key.as_ref(),
            args.ssh_src_password.as_ref(),
        ),
        Location::Local(_) => remote_pipeline::SshEndpoint::default(),
    };

    let destination_endpoint = match destination {
        Location::Remote { user, host, .. } => remote_pipeline::endpoint_for(
            user,
            host,
            args.ssh_dst_port,
            args.ssh_dst_key.as_ref(),
            args.ssh_dst_password.as_ref(),
        ),
        Location::Local(_) => remote_pipeline::SshEndpoint::default(),
    };

    let options = remote_pipeline::RemoteOptions {
        source: source_endpoint,
        destination: destination_endpoint,
        known_hosts,
        prefer_agent: args.ssh_agent,
    };

    match mode {
        TransferMode::LocalToLocal => pipeline::run_local_pipeline(args, tui, cancelled).await,
        TransferMode::LocalToRemote => {
            let (Location::Local(source_path), Location::Remote { path, .. }) =
                (source, destination)
            else {
                unreachable!("local-to-remote mode requires a remote destination");
            };
            remote_pipeline::run_upload_pipeline(
                args,
                source_path,
                &path.to_string_lossy(),
                &options,
                tui,
                &cancelled,
            )
            .await
        }
        TransferMode::RemoteToLocal => {
            let (Location::Remote { path, .. }, Location::Local(destination_path)) =
                (source, destination)
            else {
                unreachable!("remote-to-local mode requires a remote source");
            };
            remote_pipeline::run_download_pipeline(
                args,
                &path.to_string_lossy(),
                destination_path,
                &options,
                tui,
                &cancelled,
            )
            .await
        }
        TransferMode::RemoteToRemote => {
            let (
                Location::Remote {
                    path: source_path, ..
                },
                Location::Remote {
                    path: dest_path, ..
                },
            ) = (source, destination)
            else {
                unreachable!("remote-to-remote mode requires two remote locations");
            };
            remote_pipeline::run_relay_pipeline(
                args,
                &source_path.to_string_lossy(),
                &dest_path.to_string_lossy(),
                &options,
                tui,
                &cancelled,
            )
            .await
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
    use std::path::PathBuf;

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
                assert_eq!(args.exclude, Vec::<String>::new());
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
