//! Remote filesystem inspection over SSH.
//!
//! Lists the regular files below a remote directory so that the remote side
//! of a transfer can be enumerated without SFTP.

use std::path::PathBuf;

use tracing::{debug, info};

use crate::adapters::shell::shell_escape;
use crate::adapters::ssh::SshAdapter;
use crate::domain::{FileEntry, SshError};

/// Normalise a remote root path so it can be used as a path prefix.
///
/// Trailing slashes are removed; the filesystem root stays `/`.
#[must_use]
pub fn normalize_remote_root(remote_root: &str) -> String {
    let trimmed = remote_root.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Build the shell command that lists the regular files below `remote_root`.
///
/// GNU `stat` is tried first; BSD/macOS `stat` is the fallback.  Both flavours
/// print `<size> <path>` per line.
fn build_list_command(remote_root: &str) -> String {
    let root = shell_escape(remote_root);
    format!(
        "{{ cd {root} && find . -type f -exec stat -c '%s %n' {{}} + 2>/dev/null; }} \
         || {{ cd {root} && find . -type f -exec stat -f '%z %N' {{}} +; }}"
    )
}

/// Parse the `<size> <path>` lines produced by the listing command.
///
/// Paths are relative to the listed root; they are re-prefixed with
/// `root_prefix` so that callers can `strip_prefix` the remote root again.
fn parse_listing(output: &str, root_prefix: &str) -> Vec<FileEntry> {
    let mut entries = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((size_str, path)) = line.split_once(' ') else {
            debug!("skipping unparsable remote listing line: {line}");
            continue;
        };

        let Ok(size) = size_str.trim().parse::<u64>() else {
            debug!("skipping remote listing line with bad size: {line}");
            continue;
        };

        let relative = path.trim().trim_start_matches("./");
        if relative.is_empty() {
            continue;
        }

        let full_path = if root_prefix == "/" {
            PathBuf::from("/").join(relative)
        } else {
            PathBuf::from(root_prefix).join(relative)
        };

        entries.push(FileEntry::new(full_path, size));
    }

    entries
}

/// List the regular files below a remote directory.
///
/// The returned entries carry the full remote path so that they can be
/// compared against a local scan and stripped relative to the remote root.
///
/// # Errors
///
/// Returns `SshError` when the remote command fails — typically because the
/// remote path does not exist or is not a directory.
pub async fn list_remote_files(
    adapter: &SshAdapter,
    remote_root: &str,
) -> Result<Vec<FileEntry>, SshError> {
    let root = normalize_remote_root(remote_root);
    let command = build_list_command(&root);

    debug!("listing remote directory {root}");
    let output = adapter
        .exec_command(&command)
        .await
        .map_err(|e| SshError::RemoteCommand {
            command: format!("listing {root}"),
            source: std::io::Error::other(format!("{e}")),
        })?;

    let stdout = String::from_utf8_lossy(&output);
    let entries = parse_listing(&stdout, &root);
    info!("found {} regular files under {root}", entries.len());
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{build_list_command, normalize_remote_root, parse_listing};

    #[test]
    fn root_trailing_slashes_are_removed() {
        assert_eq!(normalize_remote_root("/mnt/data/"), "/mnt/data");
        assert_eq!(normalize_remote_root("/mnt/data//"), "/mnt/data");
    }

    #[test]
    fn filesystem_root_is_preserved() {
        assert_eq!(normalize_remote_root("/"), "/");
        assert_eq!(normalize_remote_root(""), "/");
    }

    #[test]
    fn listing_command_falls_back_to_bsd_stat() {
        let command = build_list_command("/mnt/data");
        assert!(command.contains("stat -c '%s %n'"));
        assert!(command.contains("stat -f '%z %N'"));
        assert!(command.contains("find . -type f"));
    }

    #[test]
    fn parses_size_and_path_pairs() {
        let listing = "12 ./a.txt\n4096 ./sub/b.bin\n";
        let entries = parse_listing(listing, "/mnt/data");
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        let sizes: Vec<u64> = entries.iter().map(|e| e.size).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/mnt/data/a.txt"),
                PathBuf::from("/mnt/data/sub/b.bin")
            ]
        );
        assert_eq!(sizes, vec![12, 4096]);
    }

    #[test]
    fn skips_blank_and_malformed_lines() {
        let listing = "\n   \nnot-a-number ./a.txt\n5 ./b.txt\n";
        let entries = parse_listing(listing, "/mnt/data");
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/mnt/data/b.txt")]);
    }

    #[test]
    fn paths_with_spaces_are_preserved() {
        let listing = "7 ./my file.txt\n";
        let entries = parse_listing(listing, "/mnt/data");
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/mnt/data/my file.txt")]);
    }

    #[test]
    fn root_prefix_is_the_filesystem_root() {
        let listing = "3 ./etc/hosts\n";
        let entries = parse_listing(listing, "/");
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("/etc/hosts")]);
    }
}
