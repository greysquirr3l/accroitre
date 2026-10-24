#![cfg(unix)]
//! End-to-end tests that drive the `accro` binary itself.
//!
//! The binary is run against an in-process SSH server, so the command line
//! surface — argument parsing, the remote dispatch and the summary — is
//! covered together.
//!
//! The remote end of an SSH transfer is driven through a POSIX shell, so the
//! test server needs one too.  These tests therefore only run on Unix.

use std::os::unix::fs::MetadataExt;

use tokio::process::Command;

#[path = "../../testkit/ssh_server.rs"]
mod ssh_server;

use ssh_server::TestServer;

/// The error type used by the tests.
type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Run the `accro` binary and return its output.
async fn run_accro(args: &[&str]) -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_accro"))
        .args(args)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "accro {args:?} failed with {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    Ok(())
}

/// Run the `accro` binary expecting it to fail.
async fn run_accro_failing(args: &[&str]) -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_accro"))
        .args(args)
        .output()
        .await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "accro {args:?} unexpectedly succeeded\nstderr: {stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn copies_a_local_tree_to_a_remote_host() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(source.join("nested"))?;
    std::fs::write(source.join("a.txt"), "hello world")?;
    std::fs::write(source.join("nested/b.bin"), vec![7u8; 4096])?;

    let source_arg = source.to_string_lossy().into_owned();
    let remote_arg = format!("tester@127.0.0.1:{}", remote.to_string_lossy());
    let port = server.port().to_string();

    run_accro(&[
        "copy",
        &source_arg,
        &remote_arg,
        "--ssh-dst-port",
        &port,
        "--ssh-dst-password",
        "secret",
        "--quiet",
    ])
    .await?;

    assert_eq!(
        std::fs::read_to_string(remote.join("a.txt"))?,
        "hello world"
    );
    assert_eq!(std::fs::read(remote.join("nested/b.bin"))?.len(), 4096);
    Ok(())
}

#[tokio::test]
async fn copies_a_remote_tree_to_the_local_disk() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let remote = tmp.path().join("remote");
    let destination = tmp.path().join("local");

    std::fs::create_dir_all(remote.join("nested"))?;
    std::fs::write(remote.join("a.txt"), "hello world")?;
    std::fs::write(remote.join("nested/b.bin"), vec![7u8; 4096])?;

    let remote_arg = format!("tester@127.0.0.1:{}", remote.to_string_lossy());
    let destination_arg = destination.to_string_lossy().into_owned();
    let port = server.port().to_string();

    run_accro(&[
        "copy",
        &remote_arg,
        &destination_arg,
        "--ssh-src-port",
        &port,
        "--ssh-src-password",
        "secret",
        "--quiet",
    ])
    .await?;

    assert_eq!(
        std::fs::read_to_string(destination.join("a.txt"))?,
        "hello world"
    );
    assert_eq!(std::fs::read(destination.join("nested/b.bin"))?.len(), 4096);
    Ok(())
}

#[tokio::test]
async fn hard_links_duplicate_content_on_the_remote_host() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("a.txt"), "duplicate content")?;
    std::fs::write(source.join("b.txt"), "duplicate content")?;

    let source_arg = source.to_string_lossy().into_owned();
    let remote_arg = format!("tester@127.0.0.1:{}", remote.to_string_lossy());
    let port = server.port().to_string();

    run_accro(&[
        "copy",
        &source_arg,
        &remote_arg,
        "--ssh-dst-port",
        &port,
        "--ssh-dst-password",
        "secret",
        "--quiet",
    ])
    .await?;

    let a_meta = std::fs::metadata(remote.join("a.txt"))?;
    let b_meta = std::fs::metadata(remote.join("b.txt"))?;
    assert_eq!(a_meta.ino(), b_meta.ino(), "duplicates must share an inode");
    Ok(())
}

#[tokio::test]
async fn reports_a_missing_remote_source() -> TestResult {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("does-not-exist");
    let destination = tmp.path().join("local");

    let remote_arg = format!("tester@127.0.0.1:{}", missing.to_string_lossy());
    let destination_arg = destination.to_string_lossy().into_owned();
    let port = server.port().to_string();

    run_accro_failing(&[
        "copy",
        &remote_arg,
        &destination_arg,
        "--ssh-src-port",
        &port,
        "--ssh-src-password",
        "secret",
        "--quiet",
    ])
    .await?;
    Ok(())
}

#[tokio::test]
async fn hashes_files_and_prints_json_records() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("a.txt");
    std::fs::write(&file, "hello world")?;

    let output = Command::new(env!("CARGO_BIN_EXE_accro"))
        .args([
            "hash",
            "--json",
            "--algorithm",
            "xxhash128",
            &file.to_string_lossy(),
        ])
        .output()
        .await?;

    assert!(output.status.success(), "the hash command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let record: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(
        record.get("path").and_then(|p| p.as_str()),
        Some(file.to_string_lossy().as_ref())
    );
    let hash = record
        .get("hash")
        .and_then(|h| h.as_str())
        .unwrap_or_default();
    assert_eq!(hash.len(), 32, "an xxHash-128 digest is 32 hex characters");
    Ok(())
}
