//! End-to-end tests for the remote copy pipelines.
//!
//! The pipelines run against an in-process SSH server, so the whole path —
//! scanning, deduplication, tar streaming and hard-linking — is exercised
//! against a real SSH transport.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use accroitre_cli::cli::CopyArgs;
use accroitre_cli::remote_pipeline::{
    KnownHostsOptions, RemoteOptions, SshEndpoint, run_download_pipeline, run_upload_pipeline,
};
use accroitre_cli::tui::TuiProgress;

#[path = "../../testkit/ssh_server.rs"]
mod ssh_server;

use ssh_server::TestServer;

/// The error type used by the tests.
type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Build the copy arguments for a transfer.
fn copy_args() -> CopyArgs {
    CopyArgs {
        source: String::new(),
        destination: String::new(),
        buffer: 1,
        threads: Some(2),
        dry_run: false,
        no_verify: true,
        no_dedup: false,
        no_cache: false,
        force: false,
        overwrite: false,
        delete: false,
        exclude: Vec::new(),
        log_file: None,
        quiet: true,
        ssh_src_port: 22,
        ssh_src_key: None,
        ssh_src_password: None,
        ssh_dst_port: 22,
        ssh_dst_key: None,
        ssh_dst_password: None,
        ssh_agent: false,
        ssh_known_hosts: None,
        ssh_strict_host_key: false,
        compress: false,
        link_strategy: "hardlink".to_owned(),
    }
}

/// Build the remote connection options for the test server.
fn remote_options(server: &TestServer) -> RemoteOptions {
    let endpoint = SshEndpoint {
        user: "tester".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: server.port(),
        key: None,
        password: Some("secret".to_owned()),
    };

    RemoteOptions {
        source: endpoint.clone(),
        destination: endpoint,
        known_hosts: KnownHostsOptions::default(),
        prefer_agent: false,
    }
}

/// Check that two files share the same inode.
#[cfg(unix)]
fn assert_same_inode(first: &std::path::Path, second: &std::path::Path) -> TestResult {
    use std::os::unix::fs::MetadataExt;

    let first_meta = std::fs::metadata(first)?;
    let second_meta = std::fs::metadata(second)?;
    assert_eq!(
        first_meta.ino(),
        second_meta.ino(),
        "duplicates must share an inode"
    );
    Ok(())
}

/// Check that two files share the same inode.
#[cfg(not(unix))]
fn assert_same_inode(_first: &std::path::Path, _second: &std::path::Path) -> TestResult {
    Ok(())
}

#[tokio::test]
async fn upload_copies_a_local_tree_to_a_remote_host() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(source.join("nested"))?;
    std::fs::write(source.join("a.txt"), "hello world")?;
    std::fs::write(source.join("nested/b.bin"), vec![7u8; 4096])?;

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_upload_pipeline(
        &args,
        &source,
        &remote.to_string_lossy(),
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await?;

    assert_eq!(result.copy_result.files_copied, 2);
    assert_eq!(result.dedup_stats.total_files, 2);
    assert!(result.copy_result.errors.is_empty());

    let copied = std::fs::read_to_string(remote.join("a.txt"))?;
    assert_eq!(copied, "hello world");
    Ok(())
}

#[tokio::test]
async fn upload_hard_links_duplicate_content_on_the_remote_host() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("a.txt"), "duplicate content")?;
    std::fs::write(source.join("b.txt"), "duplicate content")?;

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_upload_pipeline(
        &args,
        &source,
        &remote.to_string_lossy(),
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await?;

    assert_eq!(result.dedup_stats.duplicate_files, 1);
    assert_eq!(result.copy_result.files_linked, 1);
    assert_eq!(result.copy_result.files_copied, 1);

    let first = std::fs::read_to_string(remote.join("a.txt"))?;
    let second = std::fs::read_to_string(remote.join("b.txt"))?;
    assert_eq!(first, "duplicate content");
    assert_eq!(second, "duplicate content");

    assert_same_inode(&remote.join("a.txt"), &remote.join("b.txt"))?;
    Ok(())
}

#[tokio::test]
async fn download_copies_a_remote_tree_to_the_local_disk() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let remote = tmp.path().join("remote");
    let destination = tmp.path().join("local");

    std::fs::create_dir_all(remote.join("nested"))?;
    std::fs::write(remote.join("a.txt"), "hello world")?;
    std::fs::write(remote.join("nested/b.bin"), vec![7u8; 4096])?;

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_download_pipeline(
        &args,
        &remote.to_string_lossy(),
        &destination,
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await?;

    assert_eq!(result.copy_result.files_copied, 2);
    assert!(result.copy_result.errors.is_empty());

    let copied = std::fs::read_to_string(destination.join("a.txt"))?;
    assert_eq!(copied, "hello world");
    let binary = std::fs::read(destination.join("nested/b.bin"))?;
    assert_eq!(binary.len(), 4096);
    Ok(())
}

#[tokio::test]
async fn download_hard_links_duplicate_content_locally() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let remote = tmp.path().join("remote");
    let destination = tmp.path().join("local");

    std::fs::create_dir_all(&remote)?;
    std::fs::write(remote.join("a.txt"), "duplicate content")?;
    std::fs::write(remote.join("b.txt"), "duplicate content")?;

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_download_pipeline(
        &args,
        &remote.to_string_lossy(),
        &destination,
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await?;

    assert_eq!(result.dedup_stats.duplicate_files, 1);
    assert_eq!(result.copy_result.files_linked, 1);
    assert_eq!(result.copy_result.files_copied, 1);

    let first = std::fs::read_to_string(destination.join("a.txt"))?;
    let second = std::fs::read_to_string(destination.join("b.txt"))?;
    assert_eq!(first, "duplicate content");
    assert_eq!(second, "duplicate content");

    assert_same_inode(&destination.join("a.txt"), &destination.join("b.txt"))?;
    Ok(())
}

#[tokio::test]
async fn dry_run_reports_without_transferring_anything() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("a.txt"), "hello world")?;

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut args = copy_args();
    args.dry_run = true;

    let result = run_upload_pipeline(
        &args,
        &source,
        &remote.to_string_lossy(),
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await?;

    assert_eq!(result.copy_result.files_copied, 0);
    assert_eq!(result.dedup_stats.total_files, 1);
    assert!(
        !remote.exists(),
        "a dry run must not create the destination"
    );
    Ok(())
}

#[tokio::test]
async fn missing_remote_directory_is_reported() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("does-not-exist");
    let destination = tmp.path().join("local");

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_download_pipeline(
        &args,
        &missing.to_string_lossy(),
        &destination,
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await;

    assert!(result.is_err(), "a missing remote directory must fail");
    Ok(())
}

#[tokio::test]
async fn missing_local_source_is_reported() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("does-not-exist");
    let remote = tmp.path().join("remote");

    let tui = TuiProgress::new(true);
    let cancelled = Arc::new(AtomicBool::new(false));
    let args = copy_args();

    let result = run_upload_pipeline(
        &args,
        &missing,
        &remote.to_string_lossy(),
        &remote_options(&server),
        &tui,
        &cancelled,
    )
    .await;

    assert!(result.is_err(), "a missing local source must fail");
    Ok(())
}
