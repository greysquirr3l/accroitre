#![cfg(unix)]
//! End-to-end tests for the SSH transport.
//!
//! A minimal SSH server runs in-process and executes commands through the
//! local shell, so the tar streaming, listing and hashing paths are exercised
//! against a real SSH transport.
//!
//! The remote end of an SSH transfer is driven through a POSIX shell, so the
//! test server needs one too.  These tests therefore only run on Unix.

use std::path::Path;

use accroitre::adapters::remote_dedup::{RemoteHashMethod, hash_remote_files};
use accroitre::adapters::remote_fs::list_remote_files;
use accroitre::adapters::ssh::{AuthMethod, SshAdapter, SshConfig};
use accroitre::adapters::ssh_tar::{download_tar_stream, relay_tar_stream, upload_tar_stream};
use accroitre::domain::{FileEntry, Hash, HashAlgorithm, TarStreamConfig};

#[path = "../../testkit/ssh_server.rs"]
mod ssh_server;

use ssh_server::TestServer;

/// Build an adapter that talks to the test server.
fn adapter_for(server: &TestServer) -> SshAdapter {
    let mut config = SshConfig::new(
        "127.0.0.1".to_owned(),
        "tester".to_owned(),
        AuthMethod::Password("secret".to_owned()),
    );
    config.port = server.port();
    SshAdapter::new(config)
}

/// Build a `FileEntry` for a path under `root`.
fn entry(root: &Path, relative: &str, size: u64) -> FileEntry {
    FileEntry::new(root.join(relative), size)
}

/// Tar streaming configuration with a small batch size so batching is exercised.
const fn tar_config() -> TarStreamConfig {
    TarStreamConfig {
        batch_size: 64 * 1024,
        compress: false,
    }
}

#[tokio::test]
async fn upload_streams_files_to_the_remote_host() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(source.join("nested"))?;
    std::fs::write(source.join("a.txt"), "hello world")?;
    std::fs::write(source.join("nested/b.bin"), vec![7u8; 4096])?;

    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let entries = vec![
        entry(&source, "a.txt", 11),
        entry(&source, "nested/b.bin", 4096),
    ];

    let result = upload_tar_stream(
        &adapter,
        &entries,
        &source,
        &remote.to_string_lossy(),
        &tar_config(),
    )
    .await?;

    assert_eq!(result.files_transferred, 2);
    assert_eq!(result.bytes_transferred, 4107);
    assert!(
        result.is_complete(),
        "upload reported errors: {:?}",
        result.errors
    );

    let copied = std::fs::read_to_string(remote.join("a.txt"))?;
    assert_eq!(copied, "hello world");
    let binary = std::fs::read(remote.join("nested/b.bin"))?;
    assert_eq!(binary.len(), 4096);

    adapter.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn download_streams_files_from_the_remote_host() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let remote = tmp.path().join("remote");
    let destination = tmp.path().join("local");

    std::fs::create_dir_all(remote.join("nested"))?;
    std::fs::write(remote.join("a.txt"), "hello world")?;
    std::fs::write(remote.join("nested/b.bin"), vec![7u8; 4096])?;

    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let remote_root = remote.to_string_lossy().to_string();
    let entries = vec![
        entry(&remote, "a.txt", 11),
        entry(&remote, "nested/b.bin", 4096),
    ];

    let result = download_tar_stream(
        &adapter,
        &entries,
        &remote_root,
        &destination,
        &tar_config(),
    )
    .await?;

    assert_eq!(result.files_transferred, 2);
    assert!(
        result.is_complete(),
        "download reported errors: {:?}",
        result.errors
    );

    let copied = std::fs::read_to_string(destination.join("a.txt"))?;
    assert_eq!(copied, "hello world");
    let binary = std::fs::read(destination.join("nested/b.bin"))?;
    assert_eq!(binary.len(), 4096);

    adapter.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn listing_enumerates_remote_files_with_sizes() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let remote = tmp.path().join("remote");

    std::fs::create_dir_all(remote.join("nested"))?;
    std::fs::write(remote.join("a.txt"), "hello world")?;
    std::fs::write(remote.join("nested/b.bin"), vec![7u8; 4096])?;

    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let remote_root = remote.to_string_lossy().to_string();
    let entries = list_remote_files(&adapter, &remote_root).await?;

    let mut found: Vec<(String, u64)> = entries
        .iter()
        .map(|e| (e.path.to_string_lossy().into_owned(), e.size))
        .collect();
    found.sort();

    assert_eq!(found.len(), 2, "unexpected listing: {found:?}");
    let a = found.iter().find(|(path, _)| path.ends_with("a.txt"));
    let b = found.iter().find(|(path, _)| path.ends_with("b.bin"));
    assert!(a.is_some(), "a.txt missing from the listing: {found:?}");
    assert!(b.is_some(), "b.bin missing from the listing: {found:?}");
    assert_eq!(a.map(|(_, size)| *size), Some(11));
    assert_eq!(b.map(|(_, size)| *size), Some(4096));

    adapter.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn relay_moves_files_between_two_remote_hosts() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("source");
    let destination = tmp.path().join("destination");

    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("a.txt"), "hello world")?;

    let source_adapter = adapter_for(&server);
    let destination_adapter = adapter_for(&server);
    source_adapter.connect().await?;
    destination_adapter.connect().await?;

    let entries = vec![entry(&source, "a.txt", 11)];
    let result = relay_tar_stream(
        &source_adapter,
        &destination_adapter,
        &entries,
        &source.to_string_lossy(),
        &destination.to_string_lossy(),
        &tar_config(),
    )
    .await?;

    assert_eq!(result.files_transferred, 1);
    assert!(
        result.is_complete(),
        "relay reported errors: {:?}",
        result.errors
    );

    let copied = std::fs::read_to_string(destination.join("a.txt"))?;
    assert_eq!(copied, "hello world");

    source_adapter.disconnect().await?;
    destination_adapter.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn commands_can_fail_with_a_non_zero_exit_status() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let result = adapter.exec_command("exit 3").await;
    assert!(result.is_err(), "a failing command must report an error");

    adapter.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn commands_can_read_their_stdin() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start().await?;
    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let output = adapter.exec_with_stdin("cat", b"round trip").await?;

    assert_eq!(String::from_utf8_lossy(&output), "round trip");

    adapter.disconnect().await?;
    Ok(())
}

/// Write a stand-in for `accro` that answers `accro hash --json` requests.
fn write_fake_accro(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --json|--algorithm) continue ;;
        xxhash128|blake3) continue ;;
        */a.txt) printf '{"path":"%s","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}\n' "$arg" ;;
        */b.txt) printf '{"path":"%s","hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}\n' "$arg" ;;
    esac
done
"#;
    let path = directory.join("accro");
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[tokio::test]
async fn native_remote_hashing_uses_the_accro_hash_command()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let tools = tmp.path().join("tools");
    std::fs::create_dir_all(&tools)?;
    write_fake_accro(&tools)?;

    let server = TestServer::start_with_path(Some(&tools)).await?;
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote)?;
    std::fs::write(remote.join("a.txt"), "one")?;
    std::fs::write(remote.join("b.txt"), "two")?;

    let adapter = adapter_for(&server);
    adapter.connect().await?;

    let remote_root = remote.to_string_lossy().to_string();
    let entries = list_remote_files(&adapter, &remote_root).await?;

    let result =
        hash_remote_files(&adapter, &entries, &remote_root, HashAlgorithm::XxHash128).await?;

    assert_eq!(result.method, RemoteHashMethod::AccroNative);
    assert_eq!(result.hashes.len(), 2);

    // The hashes are keyed by the full path of the entry, not the relative
    // name the remote command was given.
    for entry in &entries {
        let hash = result
            .hashes
            .get(&entry.path)
            .ok_or("missing hash for the entry")?;
        assert!(matches!(hash, Hash::XxHash128(_)));
    }

    adapter.disconnect().await?;
    Ok(())
}
