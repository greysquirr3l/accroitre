//! SSH port — remote transport over SSH.

use std::path::Path;

use crate::domain::{FileEntry, SshError};

/// Port for SSH transport operations.
pub trait SshPort: Send + Sync {
    /// Establish an SSH connection to a remote host.
    fn connect(
        &self,
        host: &str,
        port: u16,
        user: &str,
    ) -> impl Future<Output = Result<(), SshError>> + Send;

    /// Execute a command on the remote host and return its stdout.
    fn exec_command(&self, command: &str)
    -> impl Future<Output = Result<Vec<u8>, SshError>> + Send;

    /// Stream a tar archive of the given entries to the remote host.
    fn stream_tar_upload(
        &self,
        entries: &[FileEntry],
        source_root: &Path,
        remote_dest: &str,
    ) -> impl Future<Output = Result<u64, SshError>> + Send;

    /// Stream a tar archive from the remote host and unpack locally.
    fn stream_tar_download(
        &self,
        remote_entries: &[FileEntry],
        remote_root: &str,
        local_dest: &Path,
    ) -> impl Future<Output = Result<u64, SshError>> + Send;

    /// Disconnect the SSH session.
    fn disconnect(&self) -> impl Future<Output = Result<(), SshError>> + Send;
}
