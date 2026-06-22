//! Destination-root exclusive lock.
//!
//! Prevents two `accro` processes from operating on the same destination at
//! the same time. Without this, concurrent runs would race on the manifest
//! (write-temp-then-rename to a shared `*.tmp` path) and corrupt each other's
//! `CopyManifest` and `SqliteCache` state.
//!
//! The lock is acquired once at pipeline start via [`DestLock::acquire`] and
//! released on drop. The lock file is `.accroitre.lock` inside `dest_root`.
//!
//! On Linux/macOS this uses POSIX advisory locks (`fcntl(F_SETLK)`); on Windows
//! it uses `LockFileEx`. The underlying [`File::try_lock`] call was stabilised
//! in Rust 1.89.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Exclusive lock on a destination root, held for the lifetime of the guard.
#[derive(Debug)]
pub struct DestLock {
    file: File,
    path: PathBuf,
}

/// Failure modes for [`DestLock::acquire`].
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another `accro` process holds the lock on this destination.
    #[error(
        "another accro process holds the lock at {path}; if no other instance is running, delete the file and retry"
    )]
    Busy {
        /// Path to the lock file.
        path: PathBuf,
    },
    /// The lock file could not be opened or created.
    #[error("failed to open lock file at {path}")]
    Open {
        /// Path to the lock file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

impl DestLock {
    /// Lock-file name within the destination root.
    pub const FILENAME: &'static str = ".accroitre.lock";

    /// Acquire an exclusive lock on `dest_root`.
    ///
    /// Creates `.accroitre.lock` if it doesn't exist, then takes an OS-level
    /// exclusive lock. Returns [`LockError::Busy`] if another process holds it.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock file cannot be opened or the lock cannot
    /// be acquired due to contention.
    pub fn acquire(dest_root: &Path) -> Result<Self, LockError> {
        let path = dest_root.join(Self::FILENAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| LockError::Open {
                path: path.clone(),
                source,
            })?;

        file.try_lock()
            .map_err(|_| LockError::Busy { path: path.clone() })?;

        Ok(Self { file, path })
    }

    /// Path to the lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DestLock {
    fn drop(&mut self) {
        // Best-effort unlock; the OS releases the lock on FD close even if
        // unlock fails (process exit, file already closed, etc.).
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn acquire_releases_on_drop() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let path = dir.path().join(DestLock::FILENAME);

        {
            let _lock = DestLock::acquire(dir.path())?;
            assert!(path.exists(), "lock file created");
        }

        assert!(!path.exists(), "lock file removed on drop");
        Ok(())
    }

    #[test]
    fn second_acquire_returns_busy() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let _first = DestLock::acquire(dir.path())?;

        let second = DestLock::acquire(dir.path());
        assert!(matches!(second, Err(LockError::Busy { .. })));
        Ok(())
    }

    #[test]
    fn re_acquire_after_release_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        {
            let _lock = DestLock::acquire(dir.path())?;
        }
        let _lock2 = DestLock::acquire(dir.path())?;
        Ok(())
    }
}
