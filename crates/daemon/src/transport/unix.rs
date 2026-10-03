//! The daemon's Unix socket, the only listener in v1 (ADR 0010).

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::UnixListener;

use crate::{Daemon, Peer};

/// A bound socket. It holds an exclusive lock next to the socket file, so a
/// second daemon refuses to start instead of stealing the socket.
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
}

impl Listener {
    /// Binds `path`, creating its directory if needed. A socket file left by
    /// a daemon that crashed is replaced; a live daemon holding the lock
    /// fails the bind.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;

        let lock = File::create(path.with_extension("lock"))?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another daemon holds {}", path.display()),
            ),
            TryLockError::Error(error) => error,
        })?;

        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            listener,
            path: path.to_owned(),
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts clients until the listener fails, serving each on its own
    /// task. The peer's uid comes from `getpeereid`, which tokio calls for
    /// `peer_cred` on macOS.
    pub async fn run(self, daemon: Arc<Daemon>) -> io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let uid = match stream.peer_cred() {
                Ok(cred) => cred.uid(),
                Err(error) => {
                    eprintln!("slopwatchd: can't read the peer's uid, dropping it: {error}");
                    continue;
                }
            };
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                if let Err(error) = daemon.serve(stream, Peer { uid }).await {
                    eprintln!("slopwatchd: connection failed: {error}");
                }
            });
        }
    }
}
