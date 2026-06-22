//! Domain error types — all use `thiserror`, no I/O framework dependencies.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that occur during directory scanning.
#[derive(Debug, Error)]
pub enum ScanError {
    #[error("failed to read directory entry in {path}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read metadata for {path}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to resolve physical offset for {path}")]
    PhysicalOffset {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("source path does not exist: {0}")]
    SourceNotFound(PathBuf),
}

/// Errors that occur during file hashing.
#[derive(Debug, Error)]
pub enum HashError {
    #[error("failed to open file for hashing: {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error while hashing {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Errors that occur during file copying.
#[derive(Debug, Error)]
pub enum CopyError {
    #[error("failed to create destination directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to copy {src} to {dst}")]
    FileCopy {
        src: PathBuf,
        dst: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create hard link from {src} to {dst}")]
    HardLink {
        src: PathBuf,
        dst: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to set permissions on {path}")]
    Permissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("insufficient space at destination: need {needed} bytes, have {available} bytes")]
    InsufficientSpace { needed: u64, available: u64 },

    #[error("tar packing failed for batch starting at {path}")]
    TarPack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("tar unpacking failed at destination {path}")]
    TarUnpack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("transport error: {message} ({path})")]
    Transport {
        /// Human-readable description of which transport step failed.
        message: String,
        /// Path of the file being transferred when the error occurred (best-effort).
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Errors that occur during post-copy verification.
#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("hash mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("size mismatch for {path}: expected {expected} bytes, got {actual} bytes")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    #[error("missing file at destination: {0}")]
    MissingFile(PathBuf),

    #[error("verification I/O error for {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Errors that occur during SSH transport.
#[derive(Debug, Error)]
pub enum SshError {
    #[error("SSH connection failed to {host}:{port}")]
    Connection {
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("SSH authentication failed for {user}@{host}")]
    Authentication { user: String, host: String },

    #[error("SSH channel error: {message}")]
    Channel {
        message: String,
        #[source]
        source: std::io::Error,
    },

    #[error("remote command failed: {command}")]
    RemoteCommand {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("SSH key error: {0}")]
    Key(String),
}

/// Errors related to pre-flight space checks.
#[derive(Debug, Error)]
pub enum SpaceError {
    #[error("failed to query available space at {path}")]
    Query {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("insufficient space at {path}: need {needed} bytes, have {available} bytes")]
    Insufficient {
        path: PathBuf,
        needed: u64,
        available: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_error_display() {
        let err = ScanError::SourceNotFound(PathBuf::from("/missing"));
        assert_eq!(err.to_string(), "source path does not exist: /missing");
    }

    #[test]
    fn hash_error_display() {
        let err = HashError::Io {
            path: PathBuf::from("/test.bin"),
            source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken"),
        };
        assert!(err.to_string().contains("/test.bin"));
    }

    #[test]
    fn copy_error_insufficient_space() {
        let err = CopyError::InsufficientSpace {
            needed: 1024,
            available: 512,
        };
        let msg = err.to_string();
        assert!(msg.contains("1024"));
        assert!(msg.contains("512"));
    }

    #[test]
    fn verify_error_hash_mismatch() {
        let err = VerifyError::HashMismatch {
            path: PathBuf::from("/foo.txt"),
            expected: "aabb".to_owned(),
            actual: "ccdd".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("aabb"));
        assert!(msg.contains("ccdd"));
    }

    #[test]
    fn ssh_error_display() {
        let err = SshError::Authentication {
            user: "nick".to_owned(),
            host: "example.com".to_owned(),
        };
        assert!(err.to_string().contains("nick@example.com"));
    }

    #[test]
    fn space_error_display() {
        let err = SpaceError::Insufficient {
            path: PathBuf::from("/mnt"),
            needed: 2048,
            available: 1024,
        };
        let msg = err.to_string();
        assert!(msg.contains("2048"));
        assert!(msg.contains("1024"));
    }

    #[test]
    fn scan_error_source_chain() {
        use std::error::Error;

        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = ScanError::ReadDir {
            path: PathBuf::from("/secret"),
            source: io_err,
        };
        // Verify source() returns the underlying io::Error
        assert!(err.source().is_some());
    }
}
