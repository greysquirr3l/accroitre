//! SSH port — remote transport over SSH.

use std::path::Path;

use crate::domain::{CopyError, FileEntry, SshError, TarStreamConfig, TarStreamResult};

/// Port for SSH transport operations.
///
/// Implementors own a single SSH session; the connection details (host, port,
/// user, authentication method) are supplied when the adapter is constructed,
/// so [`connect`](SshPort::connect) takes no arguments.
pub trait SshPort: Send + Sync {
    /// Establish the SSH connection and authenticate.
    fn connect(&self) -> impl Future<Output = Result<(), SshError>> + Send;

    /// Execute a command on the remote host and return its stdout.
    fn exec_command(&self, command: &str)
    -> impl Future<Output = Result<Vec<u8>, SshError>> + Send;

    /// Stream a tar archive of the given entries to the remote host.
    ///
    /// Archive members are named relative to `source_root`, so the remote tree
    /// mirrors the local one beneath `remote_dest`.
    fn stream_tar_upload(
        &self,
        entries: &[FileEntry],
        source_root: &Path,
        remote_dest: &str,
        config: &TarStreamConfig,
    ) -> impl Future<Output = Result<TarStreamResult, CopyError>> + Send;

    /// Stream a tar archive from the remote host and unpack it locally.
    ///
    /// `remote_entries` must contain absolute remote paths; the paths relative
    /// to `remote_root` become the local member names beneath `local_dest`.
    fn stream_tar_download(
        &self,
        remote_entries: &[FileEntry],
        remote_root: &str,
        local_dest: &Path,
        config: &TarStreamConfig,
    ) -> impl Future<Output = Result<TarStreamResult, CopyError>> + Send;

    /// Disconnect the SSH session.
    fn disconnect(&self) -> impl Future<Output = Result<(), SshError>> + Send;
}
