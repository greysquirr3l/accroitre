//! Programmatic construction of an `SshConfig` without going through the CLI.
//!
//! Use this when embedding accroitre into another application that already
//! has SSH credentials configured — for example, a backup tool that wants
//! to reuse the host's ssh-agent.
//!
//! This example only constructs and inspects the config; it does not open a
//! connection. See the CLI's `accro copy user@host:/path /local/path` for the
//! end-to-end pipeline.
//!
//! Run with: `cargo run --example ssh_config`

use std::path::PathBuf;
use std::time::Duration;

use accroitre::{AuthMethod, SshAdapter, SshConfig};

fn main() {
    // Connect using a key file. Other AuthMethod variants:
    //   AuthMethod::Agent                   — ssh-agent forwarded key
    //   AuthMethod::Password("secret".into()) — explicit password
    let config = SshConfig {
        host: "backup.example.com".to_owned(),
        port: 22,
        user: "backup".to_owned(),
        auth: AuthMethod::KeyFile {
            path: PathBuf::from("/home/me/.ssh/id_ed25519"),
            passphrase: None,
        },
        connect_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_mins(5),
        known_hosts_path: Some(PathBuf::from("/home/me/.ssh/known_hosts")),
    };

    println!(
        "configured for {}@{}:{}",
        config.user, config.host, config.port
    );

    // The adapter itself is cheap to construct — no I/O happens until
    // `connect()` is called.
    let _adapter = SshAdapter::new(config);

    // If you actually want to connect:
    //   let rt = tokio::runtime::Builder::new_current_thread()
    //       .enable_all().build()?;
    //   rt.block_on(async { adapter.connect().await })?;
}
