//! SSH transport adapter using `russh`.
//!
//! Provides an async SSH client that supports password, key-file and
//! ssh-agent authentication, connection reuse via multiplexed channels,
//! known-hosts verification, configurable timeouts, and auth retry.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Config, Handle, Msg};
use russh::keys::PublicKey;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::known_hosts;
use russh::{Channel, ChannelMsg, Disconnect, MethodSet};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::domain::{CopyError, FileEntry, SshError, TarStreamConfig, TarStreamResult};
use crate::ports::SshPort;

/// Maximum number of authentication retry attempts.
const MAX_AUTH_RETRIES: u32 = 3;

/// Default SSH port.
const DEFAULT_SSH_PORT: u16 = 22;

/// Default connection timeout.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default command timeout.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_mins(5);

/// SSH authentication method.
#[derive(Debug, Clone, Default)]
pub enum AuthMethod {
    /// Password-based authentication.
    Password(String),
    /// Key file-based authentication (path to private key, optional passphrase).
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// Delegate authentication to a running SSH agent.
    ///
    /// On Unix the agent is located through `SSH_AUTH_SOCK`; on Windows the
    /// Pageant agent is used.
    #[default]
    Agent,
}

/// How server host keys are verified against the `known_hosts` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KnownHostsPolicy {
    /// Unknown hosts are rejected; the transfer fails with an actionable error.
    Strict,
    /// Unknown hosts are accepted once and recorded in the `known_hosts` file.
    /// A host key that *changes* is always rejected.
    #[default]
    TrustOnFirstUse,
}

/// Configuration for the SSH adapter.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// Remote host.
    pub host: String,
    /// Remote port (default: 22).
    pub port: u16,
    /// Username for authentication.
    pub user: String,
    /// Authentication method.
    pub auth: AuthMethod,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Per-command timeout.
    pub command_timeout: Duration,
    /// Path to `known_hosts` file (default: `~/.ssh/known_hosts`).
    pub known_hosts_path: Option<PathBuf>,
    /// Policy applied when the server host key is unknown.
    pub known_hosts_policy: KnownHostsPolicy,
}

impl SshConfig {
    /// Create a new `SshConfig` with required fields and sensible defaults.
    #[must_use]
    pub const fn new(host: String, user: String, auth: AuthMethod) -> Self {
        Self {
            host,
            port: DEFAULT_SSH_PORT,
            user,
            auth,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            known_hosts_path: None,
            known_hosts_policy: KnownHostsPolicy::TrustOnFirstUse,
        }
    }
}

/// Error raised by the SSH client handler.
///
/// Wraps `russh`'s own error type so that host-key verification failures can
/// carry an actionable message instead of a generic "unknown key".
#[derive(Debug)]
enum ClientHandlerError {
    /// An error raised by `russh` itself.
    Ssh(russh::Error),
    /// Host key verification could not be completed.
    HostKey(String),
}

impl From<russh::Error> for ClientHandlerError {
    fn from(error: russh::Error) -> Self {
        Self::Ssh(error)
    }
}

impl std::fmt::Display for ClientHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ssh(e) => write!(f, "{e}"),
            Self::HostKey(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ClientHandlerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ssh(e) => Some(e),
            Self::HostKey(_) => None,
        }
    }
}

/// SSH client handler for `russh`.
///
/// Handles server key verification against `known_hosts`.
struct ClientHandler {
    known_hosts_path: Option<PathBuf>,
    known_hosts_policy: KnownHostsPolicy,
    host: String,
    port: u16,
}

impl ClientHandler {
    /// Verify a server host key against the configured `known_hosts` file.
    ///
    /// Returns `Ok(true)` when the key may be used for the connection.
    fn verify_server_key(&self, server_public_key: &PublicKey) -> Result<bool, SshError> {
        let Some(kh_path) = self.known_hosts_path.as_ref() else {
            // Without a known_hosts file there is nothing to verify against.
            // Accepting the key is the only option; the CLI warns about it.
            debug!("no known_hosts file configured — accepting server key on first use");
            return Ok(true);
        };

        match known_hosts::check_known_hosts_path(&self.host, self.port, server_public_key, kh_path)
        {
            Ok(true) => {
                debug!(
                    "server key for {}:{} matches known_hosts",
                    self.host, self.port
                );
                Ok(true)
            }
            Ok(false) => match self.known_hosts_policy {
                KnownHostsPolicy::Strict => {
                    warn!(
                        "unknown server key for {}:{} — rejected (strict host key checking)",
                        self.host, self.port
                    );
                    Ok(false)
                }
                KnownHostsPolicy::TrustOnFirstUse => {
                    info!(
                        "unknown server key for {}:{} — recording it in {}",
                        self.host,
                        self.port,
                        kh_path.display()
                    );
                    known_hosts::learn_known_hosts_path(
                        &self.host,
                        self.port,
                        server_public_key,
                        kh_path,
                    )
                    .map_err(|e| SshError::Key(format!("cannot record host key: {e}")))?;
                    Ok(true)
                }
            },
            Err(russh::keys::Error::KeyChanged { line }) => {
                warn!(
                    "server key for {}:{} changed since known_hosts line {line} — rejected",
                    self.host, self.port
                );
                Ok(false)
            }
            Err(e) => Err(SshError::Key(format!("known_hosts lookup failed: {e}"))),
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = ClientHandlerError;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let result = self.verify_server_key(server_public_key);
        async move {
            match result {
                Ok(ok) => Ok(ok),
                Err(e) => Err(ClientHandlerError::HostKey(e.to_string())),
            }
        }
    }
}

/// Async SSH transport adapter.
///
/// Wraps a `russh` client connection and provides multiplexed channel
/// operations over a single SSH session.
pub struct SshAdapter {
    config: SshConfig,
    handle: Mutex<Option<Handle<ClientHandler>>>,
}

impl SshAdapter {
    /// Create a new `SshAdapter` with the given configuration.
    ///
    /// Does not connect immediately — call [`connect`](Self::connect) first.
    #[must_use]
    pub fn new(config: SshConfig) -> Self {
        Self {
            config,
            handle: Mutex::new(None),
        }
    }

    /// Establish the SSH connection and authenticate.
    ///
    /// # Errors
    ///
    /// Returns `SshError::Connection` if the TCP connection fails, or
    /// `SshError::Authentication` if all auth attempts are exhausted.
    pub async fn connect(&self) -> Result<(), SshError> {
        let russh_config = Arc::new(Config {
            inactivity_timeout: Some(self.config.connect_timeout),
            ..Config::default()
        });

        let handler = ClientHandler {
            known_hosts_path: self.config.known_hosts_path.clone(),
            known_hosts_policy: self.config.known_hosts_policy,
            host: self.config.host.clone(),
            port: self.config.port,
        };

        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("connecting to {addr}");

        let mut handle = tokio::time::timeout(self.config.connect_timeout, async {
            client::connect(russh_config, &addr, handler).await
        })
        .await
        .map_err(|_| SshError::Connection {
            host: self.config.host.clone(),
            port: self.config.port,
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"),
        })?
        .map_err(|e| SshError::Connection {
            host: self.config.host.clone(),
            port: self.config.port,
            source: std::io::Error::other(e.to_string()),
        })?;

        // Authenticate with retry.
        self.authenticate(&mut handle).await?;

        let mut guard = self.handle.lock().await;
        *guard = Some(handle);
        drop(guard);

        info!("SSH connection established to {addr}");
        Ok(())
    }

    /// Execute a command on the remote host and return its stdout.
    ///
    /// Opens a new channel on the existing connection (multiplexed).
    ///
    /// # Errors
    ///
    /// Returns `SshError::Channel` if the channel cannot be opened, or
    /// `SshError::RemoteCommand` if the command fails.
    pub async fn exec_command(&self, command: &str) -> Result<Vec<u8>, SshError> {
        let guard = self.handle.lock().await;
        let handle = guard.as_ref().ok_or_else(|| SshError::Channel {
            message: "not connected".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"),
        })?;

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel {
                message: "failed to open session channel".to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

        drop(guard);

        debug!("executing remote command: {command}");
        channel
            .exec(true, command.as_bytes())
            .await
            .map_err(|e| SshError::RemoteCommand {
                command: command.to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

        // Collect stdout.
        let output = self.collect_channel_output(&mut channel).await?;

        Ok(output)
    }

    /// Stream data to a remote command's stdin.
    ///
    /// Opens a channel, executes the command, writes data to stdin, then
    /// collects the result.
    ///
    /// # Errors
    ///
    /// Returns `SshError` on channel or command failure.
    pub async fn exec_with_stdin(
        &self,
        command: &str,
        stdin_data: &[u8],
    ) -> Result<Vec<u8>, SshError> {
        let guard = self.handle.lock().await;
        let handle = guard.as_ref().ok_or_else(|| SshError::Channel {
            message: "not connected".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"),
        })?;

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel {
                message: "failed to open session channel".to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

        drop(guard);

        channel
            .exec(true, command.as_bytes())
            .await
            .map_err(|e| SshError::RemoteCommand {
                command: command.to_string(),
                source: std::io::Error::other(e.to_string()),
            })?;

        // Send data to stdin.
        channel
            .data(stdin_data)
            .await
            .map_err(|e| SshError::Channel {
                message: format!("failed to write stdin data: {e}"),
                source: std::io::Error::other(e.to_string()),
            })?;

        channel.eof().await.map_err(|e| SshError::Channel {
            message: format!("failed to send EOF: {e}"),
            source: std::io::Error::other(e.to_string()),
        })?;

        // Collect stdout.
        self.collect_channel_output(&mut channel).await
    }

    /// Disconnect the SSH session.
    ///
    /// # Errors
    ///
    /// Returns `SshError::Channel` if the disconnect message cannot be sent.
    pub async fn disconnect(&self) -> Result<(), SshError> {
        let handle_opt = {
            let mut guard = self.handle.lock().await;
            guard.take()
        };
        if let Some(handle) = handle_opt {
            handle
                .disconnect(Disconnect::ByApplication, "closing", "en")
                .await
                .map_err(|e| SshError::Channel {
                    message: "disconnect failed".to_string(),
                    source: std::io::Error::other(e.to_string()),
                })?;
            info!("SSH session disconnected");
        }
        Ok(())
    }

    /// Check whether the connection is still alive.
    #[must_use]
    pub async fn is_connected(&self) -> bool {
        let guard = self.handle.lock().await;
        guard.as_ref().is_some_and(|h| !h.is_closed())
    }

    /// Authenticate with retry logic.
    async fn authenticate(&self, handle: &mut Handle<ClientHandler>) -> Result<(), SshError> {
        for attempt in 1..=MAX_AUTH_RETRIES {
            debug!(
                "authentication attempt {attempt}/{MAX_AUTH_RETRIES} for {}@{}",
                self.config.user, self.config.host
            );

            let result = match &self.config.auth {
                AuthMethod::Password(password) => handle
                    .authenticate_password(&self.config.user, password)
                    .await
                    .map_err(|e| SshError::Key(format!("password authentication failed: {e}"))),
                AuthMethod::KeyFile { path, passphrase } => self
                    .authenticate_with_key(handle, path, passphrase.as_deref())
                    .await
                    .map_err(|e| SshError::Key(format!("key authentication failed: {e}"))),
                AuthMethod::Agent => self
                    .authenticate_with_agent(handle)
                    .await
                    .map_err(|e| SshError::Key(format!("agent authentication failed: {e}"))),
            };

            match result {
                Ok(AuthResult::Success) => {
                    info!(
                        "authenticated as {}@{} on attempt {attempt}",
                        self.config.user, self.config.host
                    );
                    return Ok(());
                }
                Ok(AuthResult::Failure { .. }) => {
                    warn!(
                        "authentication rejected for {}@{} (attempt {attempt}/{MAX_AUTH_RETRIES})",
                        self.config.user, self.config.host
                    );
                }
                Err(e) => {
                    warn!(
                        "authentication error for {}@{}: {e} (attempt {attempt}/{MAX_AUTH_RETRIES})",
                        self.config.user, self.config.host
                    );
                }
            }
        }

        Err(SshError::Authentication {
            user: self.config.user.clone(),
            host: self.config.host.clone(),
        })
    }

    /// Authenticate using the SSH agent.
    ///
    /// Every identity the agent offers is tried in turn; the first one the
    /// server accepts wins.
    async fn authenticate_with_agent(
        &self,
        handle: &mut Handle<ClientHandler>,
    ) -> Result<AuthResult, SshError> {
        let mut agent = connect_agent().await?;

        let identities = agent
            .request_identities()
            .await
            .map_err(|e| SshError::Key(format!("cannot list agent identities: {e}")))?;

        if identities.is_empty() {
            return Err(SshError::Key("ssh-agent holds no identities".to_string()));
        }

        let mut last_failure = None;
        for identity in &identities {
            let result = match identity {
                AgentIdentity::PublicKey { key, .. } => {
                    handle
                        .authenticate_publickey_with(
                            &self.config.user,
                            key.clone(),
                            None,
                            &mut agent,
                        )
                        .await
                }
                AgentIdentity::Certificate { certificate, .. } => {
                    handle
                        .authenticate_certificate_with(
                            &self.config.user,
                            certificate.clone(),
                            None,
                            &mut agent,
                        )
                        .await
                }
            };

            match result {
                Ok(AuthResult::Success) => return Ok(AuthResult::Success),
                Ok(failure) => {
                    debug!("agent identity rejected: {failure:?}");
                    last_failure = Some(failure);
                }
                Err(e) => {
                    debug!("agent identity failed to sign: {e}");
                    last_failure = Some(AuthResult::Failure {
                        remaining_methods: MethodSet::empty(),
                        partial_success: false,
                    });
                }
            }
        }

        Ok(last_failure.unwrap_or_else(|| AuthResult::Failure {
            remaining_methods: MethodSet::empty(),
            partial_success: false,
        }))
    }

    /// Authenticate using an SSH key file.
    async fn authenticate_with_key(
        &self,
        handle: &mut Handle<ClientHandler>,
        key_path: &Path,
        passphrase: Option<&str>,
    ) -> Result<AuthResult, russh::Error> {
        let key_pair =
            russh::keys::load_secret_key(key_path, passphrase).map_err(russh::Error::Keys)?;
        let key_with_alg = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key_pair), None);
        handle
            .authenticate_publickey(&self.config.user, key_with_alg)
            .await
    }

    /// Collect all stdout data from a channel until EOF/close.
    async fn collect_channel_output(
        &self,
        channel: &mut Channel<Msg>,
    ) -> Result<Vec<u8>, SshError> {
        let mut output = Vec::new();

        let result = tokio::time::timeout(self.config.command_timeout, async {
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        output.extend_from_slice(&data);
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    ChannelMsg::ExitStatus { exit_status } if exit_status != 0 => {
                        return Err(SshError::RemoteCommand {
                            command: format!("exit status: {exit_status}"),
                            source: std::io::Error::other(format!(
                                "remote command exited with status {exit_status}"
                            )),
                        });
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => Ok(output),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(SshError::RemoteCommand {
                command: "timeout".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"),
            }),
        }
    }
}

/// Connect to the local SSH agent.
///
/// On Unix the agent socket is located through `SSH_AUTH_SOCK`; on Windows the
/// Pageant agent is used.
#[cfg(unix)]
async fn connect_agent() -> Result<AgentClient<tokio::net::UnixStream>, SshError> {
    AgentClient::connect_env()
        .await
        .map_err(|e| SshError::Key(format!("cannot connect to ssh-agent: {e}")))
}

/// Connect to the local SSH agent.
///
/// On Windows the Pageant agent is used.
#[cfg(windows)]
async fn connect_agent() -> Result<AgentClient<pageant::PageantStream>, SshError> {
    AgentClient::connect_pageant()
        .await
        .map_err(|e| SshError::Key(format!("cannot connect to ssh-agent: {e}")))
}

impl SshPort for SshAdapter {
    async fn connect(&self) -> Result<(), SshError> {
        Self::connect(self).await
    }

    async fn exec_command(&self, command: &str) -> Result<Vec<u8>, SshError> {
        Self::exec_command(self, command).await
    }

    async fn stream_tar_upload(
        &self,
        entries: &[FileEntry],
        source_root: &Path,
        remote_dest: &str,
        config: &TarStreamConfig,
    ) -> Result<TarStreamResult, CopyError> {
        crate::adapters::ssh_tar::upload_tar_stream(self, entries, source_root, remote_dest, config)
            .await
    }

    async fn stream_tar_download(
        &self,
        remote_entries: &[FileEntry],
        remote_root: &str,
        local_dest: &Path,
        config: &TarStreamConfig,
    ) -> Result<TarStreamResult, CopyError> {
        crate::adapters::ssh_tar::download_tar_stream(
            self,
            remote_entries,
            remote_root,
            local_dest,
            config,
        )
        .await
    }

    async fn disconnect(&self) -> Result<(), SshError> {
        Self::disconnect(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_defaults() {
        let config = SshConfig::new(
            "example.com".to_string(),
            "user".to_string(),
            AuthMethod::Password("pass".to_string()),
        );

        assert_eq!(config.host, "example.com");
        assert_eq!(config.port, 22);
        assert_eq!(config.user, "user");
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.command_timeout, Duration::from_mins(5));
        assert!(config.known_hosts_path.is_none());
    }

    #[test]
    fn auth_method_key_file() {
        let auth = AuthMethod::KeyFile {
            path: PathBuf::from("/home/user/.ssh/id_ed25519"),
            passphrase: None,
        };
        // Verify the path is stored correctly.
        if let AuthMethod::KeyFile { path, passphrase } = &auth {
            assert_eq!(path, Path::new("/home/user/.ssh/id_ed25519"));
            assert!(passphrase.is_none());
        }
    }

    #[test]
    fn auth_method_key_file_with_passphrase() {
        let auth = AuthMethod::KeyFile {
            path: PathBuf::from("/home/user/.ssh/id_rsa"),
            passphrase: Some("secret".to_string()),
        };
        if let AuthMethod::KeyFile { path, passphrase } = &auth {
            assert_eq!(path, Path::new("/home/user/.ssh/id_rsa"));
            assert_eq!(passphrase.as_deref(), Some("secret"));
        }
    }

    #[tokio::test]
    async fn connect_timeout_on_unreachable_host() -> Result<(), Box<dyn std::error::Error>> {
        let config = SshConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET, guaranteed unreachable.
            port: 22,
            user: "nobody".to_string(),
            auth: AuthMethod::Password("x".to_string()),
            connect_timeout: Duration::from_millis(500),
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            known_hosts_path: None,
            known_hosts_policy: KnownHostsPolicy::TrustOnFirstUse,
        };

        let adapter = SshAdapter::new(config);
        let result = adapter.connect().await;
        assert!(result.is_err());

        if let Err(SshError::Connection { host, port, .. }) = result {
            assert_eq!(host, "192.0.2.1");
            assert_eq!(port, 22);
        }
        Ok(())
    }
}
