//! A minimal SSH server that runs commands through the local shell.
//!
//! Shared by the library and CLI integration tests.  The server accepts any
//! authentication method, executes each command with `/bin/sh -c` and pipes
//! the command's stdin and stdout through the SSH channel — which is enough
//! to exercise the real tar streaming, listing and hashing code paths.

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use russh::keys::PrivateKey;
use russh::server::{Auth, Config, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::ChildStdin;

/// The error type of [`ssh_key`], re-exported for the test helpers.
pub type KeyError = russh::keys::ssh_key::Error;

/// The error type of the SSH server.
#[derive(Debug)]
pub enum TestServerError {
    /// The listening socket could not be bound.
    Bind(std::io::Error),
    /// The host key could not be generated.
    HostKey(KeyError),
}

impl std::fmt::Display for TestServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(source) => write!(f, "cannot bind the test server: {source}"),
            Self::HostKey(source) => write!(f, "cannot generate the test host key: {source}"),
        }
    }
}

impl std::error::Error for TestServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(source) => Some(source),
            Self::HostKey(source) => Some(source),
        }
    }
}

/// A deterministic RNG used to derive the test server's host key.
pub struct TestRng(u64);

impl russh::keys::ssh_key::rand_core::TryRng for TestRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(u32::try_from(self.try_next_u64()?).unwrap_or(u32::MAX))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        Ok(x.wrapping_mul(0x2545_F491_4F6C_DD1D))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dest.chunks_mut(8) {
            let bytes = self.try_next_u64()?.to_le_bytes();
            let len = chunk.len();
            let Some(head) = bytes.get(..len) else {
                return Ok(());
            };
            chunk.copy_from_slice(head);
        }
        Ok(())
    }
}

impl russh::keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

/// Generate the host key used by the test server.
pub fn host_key() -> Result<PrivateKey, KeyError> {
    let mut rng = TestRng(0x1234_5678_9ABC_DEF0);
    PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519)
}

/// The in-process SSH server: every command runs through the local shell.
pub struct ShellServer {
    channels: HashMap<ChannelId, Channel<Msg>>,
    children: HashMap<ChannelId, Option<ChildStdin>>,
    path: OsString,
}

impl Default for ShellServer {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
            children: HashMap::new(),
            path: std::env::var_os("PATH").unwrap_or_default(),
        }
    }
}

impl Handler for ShellServer {
    type Error = russh::Error;

    fn auth_none(&mut self, _user: &str) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        std::future::ready(Ok(Auth::Accept))
    }

    fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        std::future::ready(Ok(Auth::Accept))
    }

    fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::PublicKey,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        std::future::ready(Ok(Auth::Accept))
    }

    fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        self.channels.insert(channel.id(), channel);
        std::future::ready(Ok(true))
    }

    fn exec_request(
        &mut self,
        channel_id: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let _ = session.channel_success(channel_id);
        let command = String::from_utf8_lossy(command).into_owned();

        let Some(channel) = self.channels.remove(&channel_id) else {
            return std::future::ready(Ok(()));
        };
        let (_read_half, write_half) = channel.split();

        let child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("PATH", &self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let Ok(mut child) = child else {
            return std::future::ready(Ok(()));
        };

        let stdin = child.stdin.take();
        let Some(mut stdout) = child.stdout.take() else {
            return std::future::ready(Ok(()));
        };
        self.children.insert(channel_id, stdin);

        tokio::spawn(async move {
            let mut buffer = vec![0u8; 32 * 1024];
            while let Ok(read) = stdout.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let Some(chunk) = buffer.get(..read) else {
                    break;
                };
                if write_half.data(chunk).await.is_err() {
                    break;
                }
            }

            let status = child.wait().await;
            let code = status
                .ok()
                .and_then(|status| status.code())
                .unwrap_or_default();
            let code = u32::try_from(code).unwrap_or(1);
            let _ = write_half.exit_status(code).await;
            let _ = write_half.eof().await;
            let _ = write_half.close().await;
        });

        std::future::ready(Ok(()))
    }

    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(Some(stdin)) = self.children.get_mut(&channel_id) {
            let _ = stdin.write_all(data).await;
            let _ = stdin.flush().await;
        }
        Ok(())
    }

    fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        // Dropping the stdin handle closes the pipe, which the remote command
        // sees as end-of-input.
        if let Some(slot) = self.children.get_mut(&channel_id) {
            *slot = None;
        }
        std::future::ready(Ok(()))
    }
}

/// A running test server.
pub struct TestServer {
    addr: SocketAddr,
    /// The `PATH` given to the commands the server runs.
    #[allow(dead_code)]
    path: OsString,
}

impl TestServer {
    /// Start the server on an ephemeral loopback port.
    ///
    /// # Errors
    ///
    /// Returns an error when the listening socket cannot be bound or the host
    /// key cannot be generated.
    pub async fn start() -> Result<Self, TestServerError> {
        Self::start_with_path(None).await
    }

    /// Start the server with `prefix` prepended to the `PATH` of every command.
    ///
    /// This makes it possible to shadow the tools the server finds — most
    /// importantly `accro` — with test doubles.
    ///
    /// # Errors
    ///
    /// Returns an error when the listening socket cannot be bound or the host
    /// key cannot be generated.
    pub async fn start_with_path(prefix: Option<&Path>) -> Result<Self, TestServerError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(TestServerError::Bind)?;
        let addr = listener.local_addr().map_err(TestServerError::Bind)?;

        let key = host_key().map_err(TestServerError::HostKey)?;
        let config = Arc::new(Config {
            inactivity_timeout: None,
            keys: vec![key],
            ..Config::default()
        });

        let base = std::env::var_os("PATH").unwrap_or_default();
        let mut path = prefix.map_or_else(OsString::new, |prefix| {
            let mut path = OsString::from(prefix);
            path.push(":");
            path.push(&base);
            path
        });
        if path.is_empty() {
            path.push(&base);
        }
        let server_path = path.clone();

        tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let config = Arc::clone(&config);
                let path = server_path.clone();
                tokio::spawn(async move {
                    let handler = ShellServer {
                        channels: HashMap::new(),
                        children: HashMap::new(),
                        path,
                    };
                    let _ = russh::server::run_stream(config, stream, handler).await;
                });
            }
        });

        Ok(Self { addr, path })
    }

    /// The `PATH` the server gives to the commands it runs.
    #[allow(dead_code)]
    pub fn path(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }

    /// The port the server listens on.
    pub const fn port(&self) -> u16 {
        self.addr.port()
    }
}
