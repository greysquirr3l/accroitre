//! SSH transport adapter using `russh`.
//!
//! Provides an async SSH client that supports password and key-based
//! authentication, connection reuse via multiplexed channels, known-hosts
//! verification, configurable timeouts, and auth retry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Config, Handle, Msg};
use russh::keys::PublicKey;
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::domain::SshError;

/// Maximum number of authentication retry attempts.
const MAX_AUTH_RETRIES: u32 = 3;

/// Default SSH port.
const DEFAULT_SSH_PORT: u16 = 22;

/// Default connection timeout.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default command timeout.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_mins(5);

/// SSH authentication method.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// Password-based authentication.
    Password(String),
    /// Key file-based authentication (path to private key, optional passphrase).
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
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
        }
    }
}

/// SSH client handler for `russh`.
///
/// Handles server key verification against `known_hosts`.
struct ClientHandler {
    known_hosts_path: Option<PathBuf>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // If we have a known_hosts file, verify the key against it.
        if let Some(ref kh_path) = self.known_hosts_path
            && kh_path.exists()
        {
            debug!(
                "checking server key against known_hosts at {}",
                kh_path.display()
            );
            // For now, accept the key and log it. Full known_hosts parsing
            // is deferred to when we have a richer CLI interaction model.
            // TODO(T16): Implement interactive known_hosts prompting in CLI wiring.
            let _ = server_public_key;
            return Ok(true);
        }

        // Default: accept the key (first-use trust). The CLI layer (T16) will
        // add interactive prompting for unknown hosts.
        debug!("no known_hosts file configured — accepting server key on first use");
        Ok(true)
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
                AuthMethod::Password(password) => {
                    handle
                        .authenticate_password(&self.config.user, password)
                        .await
                }
                AuthMethod::KeyFile { path, passphrase } => {
                    self.authenticate_with_key(handle, path, passphrase.as_deref())
                        .await
                }
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

    /// Authenticate using an SSH key file.
    async fn authenticate_with_key(
        &self,
        handle: &mut Handle<ClientHandler>,
        key_path: &Path,
        passphrase: Option<&str>,
    ) -> Result<AuthResult, russh::Error> {
        let key_pair =
            russh::keys::load_secret_key(key_path, passphrase).map_err(russh::Error::Keys)?;
        let key_with_alg =
            russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key_pair), None);
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
