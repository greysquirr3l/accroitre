//! Remote transfer pipelines — local↔remote and remote↔remote copies over SSH.
//!
//! Every remote transfer follows the same shape:
//!
//! 1. enumerate the source side (local scan or remote listing),
//! 2. hash the source side (locally, or on the remote host),
//! 3. build a dedup plan so only unique content crosses the wire,
//! 4. stream the unique files as tar archives,
//! 5. materialise the duplicates as hard links on the destination side.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use accroitre::adapters::remote_dedup::hash_remote_files;
use accroitre::adapters::remote_fs::{list_remote_files, normalize_remote_root};
use accroitre::adapters::shell::shell_escape;
use accroitre::adapters::ssh::{AuthMethod, KnownHostsPolicy, SshAdapter, SshConfig};
use accroitre::adapters::ssh_tar::{download_tar_stream, relay_tar_stream, upload_tar_stream};
use accroitre::domain::{CopyPlan, FileEntry, HashAlgorithm, TarStreamConfig, TarStreamResult};
use accroitre::engine::copy::CopyResult;
use accroitre::engine::dedup::build_dedup_plan_from_hashes;
use accroitre::ports::ProgressPort;
use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::cli::CopyArgs;
use crate::pipeline::{
    PipelineResult, cancelled_result, dry_run_result, parse_excludes, run_dedup, run_scan,
};
use crate::tui::TuiProgress;

/// Private key file names tried when the user did not name one.
const DEFAULT_KEY_NAMES: [&str; 4] = ["id_ed25519", "id_ecdsa", "id_rsa", "id_ed25519_sk"];

/// Connection details for one side of a remote transfer.
#[derive(Debug, Clone, Default)]
pub struct SshEndpoint {
    /// Username to authenticate as.
    pub user: String,
    /// Hostname or IP address.
    pub host: String,
    /// TCP port of the SSH service.
    pub port: u16,
    /// Private key file to authenticate with.
    pub key: Option<PathBuf>,
    /// Password to authenticate with.
    pub password: Option<String>,
}

/// How server host keys are checked for remote transfers.
#[derive(Debug, Clone, Default)]
pub struct KnownHostsOptions {
    /// Explicit path to the `known_hosts` file.
    pub path: Option<PathBuf>,
    /// Reject hosts that are not yet in the `known_hosts` file.
    pub strict: bool,
}

/// Everything needed to reach the remote hosts involved in a transfer.
#[derive(Debug, Clone, Default)]
pub struct RemoteOptions {
    /// Connection details for the source host.
    pub source: SshEndpoint,
    /// Connection details for the destination host.
    pub destination: SshEndpoint,
    /// Known-hosts configuration shared by both sides.
    pub known_hosts: KnownHostsOptions,
    /// Prefer the SSH agent over the default private key.
    pub prefer_agent: bool,
}

/// The user's home directory, if it can be determined.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The first default private key that exists, if any.
fn default_key_path() -> Option<PathBuf> {
    let home = home_dir()?;
    for name in DEFAULT_KEY_NAMES {
        let candidate = home.join(".ssh").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Choose the authentication method for an endpoint.
///
/// An explicit key or password always wins.  Otherwise the first default
/// private key is used, falling back to the SSH agent.
#[must_use]
pub fn resolve_auth(endpoint: &SshEndpoint, prefer_agent: bool) -> AuthMethod {
    if let Some(path) = &endpoint.key {
        return AuthMethod::KeyFile {
            path: path.clone(),
            passphrase: None,
        };
    }

    if let Some(password) = &endpoint.password {
        return AuthMethod::Password(password.clone());
    }

    if prefer_agent {
        return AuthMethod::Agent;
    }

    default_key_path().map_or(AuthMethod::Agent, |path| AuthMethod::KeyFile {
        path,
        passphrase: None,
    })
}

/// Resolve the `known_hosts` file to use.
///
/// An explicit path always wins; otherwise `~/.ssh/known_hosts` is used when
/// it exists.
#[must_use]
pub fn resolve_known_hosts(options: &KnownHostsOptions) -> Option<PathBuf> {
    if let Some(path) = &options.path {
        return Some(path.clone());
    }

    let home = home_dir()?;
    let default = home.join(".ssh").join("known_hosts");
    default.is_file().then_some(default)
}

/// Build the adapter configuration for an endpoint.
fn build_ssh_config(endpoint: &SshEndpoint, options: &RemoteOptions) -> SshConfig {
    let mut config = SshConfig::new(
        endpoint.host.clone(),
        endpoint.user.clone(),
        resolve_auth(endpoint, options.prefer_agent),
    );
    config.port = endpoint.port;
    config.known_hosts_path = resolve_known_hosts(&options.known_hosts);
    config.known_hosts_policy = if options.known_hosts.strict {
        KnownHostsPolicy::Strict
    } else {
        KnownHostsPolicy::TrustOnFirstUse
    };
    config
}

/// Open an SSH connection.
async fn connect(endpoint: &SshEndpoint, options: &RemoteOptions) -> Result<SshAdapter> {
    let config = build_ssh_config(endpoint, options);
    let adapter = SshAdapter::new(config);
    adapter
        .connect()
        .await
        .with_context(|| format!("cannot connect to {}@{}", endpoint.user, endpoint.host))?;
    Ok(adapter)
}

/// The entries of a plan that carry unique content.
///
/// These are the only files that need to cross the wire; the duplicates are
/// materialised as hard links afterwards.
fn canonical_entries(plan: &CopyPlan) -> Vec<&FileEntry> {
    let mut duplicates: HashSet<usize> = HashSet::new();
    for group in &plan.dedup_groups {
        duplicates.extend(group.duplicates.iter().copied());
    }

    plan.entries
        .iter()
        .enumerate()
        .filter(|(index, _)| !duplicates.contains(index))
        .map(|(_, entry)| entry)
        .collect()
}

/// Relative name of a plan entry below `root`.
fn relative_name(path: &Path, root: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Name of a plan entry relative to the prefix it was enumerated under.
fn relative_to_prefix(path: &Path, prefix: &str) -> Option<String> {
    path.strip_prefix(prefix)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Create hard links for the duplicate entries of a plan on a remote host.
///
/// The links are created in one command so that a large plan costs a single
/// round trip.  A failure is reported as a transport error.
async fn link_remote_duplicates(
    adapter: &SshAdapter,
    plan: &CopyPlan,
    source_prefix: &str,
    remote_dest: &str,
) -> Result<u64> {
    let mut commands: Vec<String> = Vec::new();
    let mut linked = 0u64;

    for group in &plan.dedup_groups {
        let Some(canonical) = plan.entries.get(group.canonical) else {
            continue;
        };
        let Some(canonical_rel) = relative_to_prefix(&canonical.path, source_prefix) else {
            continue;
        };

        let canonical_remote = format!("{}/{}", remote_dest.trim_end_matches('/'), canonical_rel);

        for duplicate_index in &group.duplicates {
            let Some(duplicate) = plan.entries.get(*duplicate_index) else {
                continue;
            };
            let Some(duplicate_rel) = relative_to_prefix(&duplicate.path, source_prefix) else {
                continue;
            };
            let duplicate_remote =
                format!("{}/{}", remote_dest.trim_end_matches('/'), duplicate_rel);
            commands.push(format!(
                "ln -f {} {}",
                shell_escape(&canonical_remote),
                shell_escape(&duplicate_remote)
            ));
            linked += 1;
        }
    }

    if commands.is_empty() {
        return Ok(0);
    }

    let command = commands.join(" && ");
    adapter
        .exec_command(&command)
        .await
        .with_context(|| format!("cannot create hard links under {remote_dest}"))?;
    info!("created {linked} remote hard links under {remote_dest}");
    Ok(linked)
}

/// Create hard links for the duplicate entries of a plan on the local disk.
fn link_local_duplicates(plan: &CopyPlan, source_root: &Path, dest_root: &Path) -> u64 {
    let mut linked = 0u64;

    for group in &plan.dedup_groups {
        let Some(canonical) = plan.entries.get(group.canonical) else {
            continue;
        };
        let Some(canonical_rel) = relative_name(&canonical.path, source_root) else {
            continue;
        };
        let canonical_local = dest_root.join(canonical_rel);

        for duplicate_index in &group.duplicates {
            let Some(duplicate) = plan.entries.get(*duplicate_index) else {
                continue;
            };
            let Some(duplicate_rel) = relative_name(&duplicate.path, source_root) else {
                continue;
            };
            let duplicate_local = dest_root.join(duplicate_rel);

            if let Some(parent) = duplicate_local.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                warn!("cannot create {}: {e}", parent.display());
                continue;
            }

            match std::fs::hard_link(&canonical_local, &duplicate_local) {
                Ok(()) => linked += 1,
                Err(e) => {
                    warn!(
                        "cannot hard-link {} → {}: {e}",
                        canonical_local.display(),
                        duplicate_local.display()
                    );
                }
            }
        }
    }

    linked
}

/// Turn a tar streaming result into a copy result.
fn copy_result_from_tar(result: TarStreamResult, links: u64) -> CopyResult {
    CopyResult {
        files_copied: result.files_transferred,
        files_linked: links,
        files_cloned: 0,
        files_symlinked: 0,
        files_fallback_copied: 0,
        bytes_copied: result.bytes_transferred,
        errors: result.errors,
    }
}

/// Build the tar streaming configuration from the CLI arguments.
const fn tar_config(args: &CopyArgs) -> TarStreamConfig {
    TarStreamConfig {
        batch_size: 100 * 1024 * 1024,
        compress: args.compress,
    }
}

/// Copy a local directory tree to a remote host.
///
/// # Errors
///
/// Returns an error when the local source cannot be scanned or the SSH
/// transfer fails.
pub async fn run_upload_pipeline(
    args: &CopyArgs,
    source: &Path,
    remote_dest: &str,
    options: &RemoteOptions,
    tui: &TuiProgress,
    cancelled: &Arc<AtomicBool>,
) -> Result<PipelineResult> {
    if !source.exists() {
        bail!("source path does not exist: {}", source.display());
    }

    let exclude_patterns = parse_excludes(&args.exclude)?;
    let buffer_size = args.buffer as usize * 1024 * 1024;
    let algorithm = HashAlgorithm::XxHash128;

    let scan_result = run_scan(source, exclude_patterns, tui).await?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(cancelled_result());
    }

    let (plan, dedup_stats) = run_dedup(
        args,
        scan_result,
        source,
        Path::new(remote_dest),
        algorithm,
        buffer_size,
        tui,
    );

    if args.dry_run {
        return Ok(dry_run_result(&plan, dedup_stats));
    }

    let adapter = connect(&options.destination, options).await?;

    let canonical: Vec<FileEntry> = canonical_entries(&plan).into_iter().cloned().collect();
    let upload =
        upload_tar_stream(&adapter, &canonical, source, remote_dest, &tar_config(args)).await;

    let result = match upload {
        Ok(result) => result,
        Err(e) => {
            let _ = adapter.disconnect().await;
            return Err(anyhow::Error::new(e).context("upload failed"));
        }
    };

    let links = link_remote_duplicates(&adapter, &plan, &source.to_string_lossy(), remote_dest)
        .await
        .unwrap_or_else(|e| {
            warn!("cannot create remote hard links: {e}");
            0
        });

    let _ = adapter.disconnect().await;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "copy" });

    Ok(PipelineResult {
        copy_result: copy_result_from_tar(result, links),
        dedup_stats,
        verify_result: None,
        cancelled: cancelled.load(Ordering::Relaxed),
    })
}

/// Copy a remote directory tree to the local disk.
///
/// # Errors
///
/// Returns an error when the remote listing, hashing or the transfer fails.
pub async fn run_download_pipeline(
    args: &CopyArgs,
    remote_source: &str,
    destination: &Path,
    options: &RemoteOptions,
    tui: &TuiProgress,
    cancelled: &Arc<AtomicBool>,
) -> Result<PipelineResult> {
    let remote_root = normalize_remote_root(remote_source);

    std::fs::create_dir_all(destination)
        .with_context(|| format!("cannot create destination: {}", destination.display()))?;

    let adapter = connect(&options.source, options).await?;

    let entries = list_remote_files(&adapter, &remote_root)
        .await
        .with_context(|| format!("cannot list remote directory {remote_root}"))?;
    if cancelled.load(Ordering::Relaxed) {
        let _ = adapter.disconnect().await;
        return Ok(cancelled_result());
    }

    let algorithm = HashAlgorithm::XxHash128;
    let hash_result = hash_remote_files(&adapter, &entries, &remote_root, algorithm)
        .await
        .context("cannot hash remote files")?;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "hash" });

    let mut hashed_entries = entries;
    for entry in &mut hashed_entries {
        entry.hash = hash_result.hashes.get(&entry.path).cloned();
    }

    let (plan, dedup_stats) =
        build_dedup_plan_from_hashes(hashed_entries, Path::new(&remote_root), destination);

    if args.dry_run {
        let _ = adapter.disconnect().await;
        return Ok(dry_run_result(&plan, dedup_stats));
    }

    let canonical: Vec<FileEntry> = canonical_entries(&plan).into_iter().cloned().collect();
    let download = download_tar_stream(
        &adapter,
        &canonical,
        &remote_root,
        destination,
        &tar_config(args),
    )
    .await;

    let result = match download {
        Ok(result) => result,
        Err(e) => {
            let _ = adapter.disconnect().await;
            return Err(anyhow::Error::new(e).context("download failed"));
        }
    };

    let links = link_local_duplicates(&plan, Path::new(&remote_root), destination);
    let _ = adapter.disconnect().await;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "copy" });

    Ok(PipelineResult {
        copy_result: copy_result_from_tar(result, links),
        dedup_stats,
        verify_result: None,
        cancelled: cancelled.load(Ordering::Relaxed),
    })
}

/// Relay a remote directory tree to another remote host.
///
/// The archive is streamed source → local → destination, so neither side needs
/// direct network access to the other.
///
/// # Errors
///
/// Returns an error when the remote listing, hashing or the relay fails.
pub async fn run_relay_pipeline(
    args: &CopyArgs,
    remote_source: &str,
    remote_dest: &str,
    options: &RemoteOptions,
    tui: &TuiProgress,
    cancelled: &Arc<AtomicBool>,
) -> Result<PipelineResult> {
    let remote_root = normalize_remote_root(remote_source);

    let source_adapter = connect(&options.source, options).await?;
    let dest_adapter = connect(&options.destination, options).await?;

    let entries = list_remote_files(&source_adapter, &remote_root)
        .await
        .with_context(|| format!("cannot list remote directory {remote_root}"))?;
    if cancelled.load(Ordering::Relaxed) {
        let _ = source_adapter.disconnect().await;
        let _ = dest_adapter.disconnect().await;
        return Ok(cancelled_result());
    }

    let algorithm = HashAlgorithm::XxHash128;
    let hash_result = hash_remote_files(&source_adapter, &entries, &remote_root, algorithm)
        .await
        .context("cannot hash remote files")?;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "hash" });

    let mut hashed_entries = entries;
    for entry in &mut hashed_entries {
        entry.hash = hash_result.hashes.get(&entry.path).cloned();
    }

    let (plan, dedup_stats) = build_dedup_plan_from_hashes(
        hashed_entries,
        Path::new(&remote_root),
        Path::new(remote_dest),
    );

    if args.dry_run {
        let _ = source_adapter.disconnect().await;
        let _ = dest_adapter.disconnect().await;
        return Ok(dry_run_result(&plan, dedup_stats));
    }

    let canonical: Vec<FileEntry> = canonical_entries(&plan).into_iter().cloned().collect();
    let relay = relay_tar_stream(
        &source_adapter,
        &dest_adapter,
        &canonical,
        &remote_root,
        remote_dest,
        &tar_config(args),
    )
    .await;

    let result = match relay {
        Ok(result) => result,
        Err(e) => {
            let _ = source_adapter.disconnect().await;
            let _ = dest_adapter.disconnect().await;
            return Err(anyhow::Error::new(e).context("relay failed"));
        }
    };

    let links = link_remote_duplicates(&dest_adapter, &plan, &remote_root, remote_dest)
        .await
        .unwrap_or_else(|e| {
            warn!("cannot create remote hard links: {e}");
            0
        });

    let _ = source_adapter.disconnect().await;
    let _ = dest_adapter.disconnect().await;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "copy" });

    Ok(PipelineResult {
        copy_result: copy_result_from_tar(result, links),
        dedup_stats,
        verify_result: None,
        cancelled: cancelled.load(Ordering::Relaxed),
    })
}

/// Build the endpoint description for a remote location.
#[must_use]
pub fn endpoint_for(
    user: &str,
    host: &str,
    port: u16,
    key: Option<&PathBuf>,
    password: Option<&String>,
) -> SshEndpoint {
    SshEndpoint {
        user: user.to_owned(),
        host: host.to_owned(),
        port,
        key: key.cloned(),
        password: password.cloned(),
    }
}
