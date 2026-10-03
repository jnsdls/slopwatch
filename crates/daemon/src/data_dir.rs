//! The directory a daemon owns: its socket now, its database and journals
//! later.

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// A data dir this process holds an exclusive `flock` on (ADR 0009). A
/// second daemon pointed at the same directory fails to lock it and exits,
/// so two daemons never share state. The lock goes when this value drops or
/// the process dies.
pub struct DataDir {
    path: PathBuf,
    _lock: File,
}

impl DataDir {
    /// Creates `path` if needed, readable only by this user, and locks it.
    /// Fails with [`io::ErrorKind::ResourceBusy`] if another process holds
    /// the lock.
    pub fn lock(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
        let lock = File::open(&path)?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("another daemon holds {}", path.display()),
            ),
            TryLockError::Error(error) => error,
        })?;
        Ok(Self { path, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn socket_path(&self) -> PathBuf {
        slopwatch_protocol::socket_path(&self.path)
    }
}
