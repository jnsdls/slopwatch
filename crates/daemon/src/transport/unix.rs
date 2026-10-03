//! The daemon's Unix socket, the only listener in v1 (ADR 0010).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;

use crate::{Daemon, DataDir, Peer};

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The socket in a locked data dir.
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Binds the socket in `data_dir`. Holding the data dir's lock means no
    /// live daemon owns the socket, so a socket file left by a daemon that
    /// crashed is replaced.
    pub fn bind(data_dir: &DataDir) -> io::Result<Self> {
        let path = data_dir.socket_path();
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts clients forever, serving each on its own task. The peer's uid
    /// comes from `getpeereid`, which tokio calls for `peer_cred` on macOS.
    pub async fn run(self, daemon: Arc<Daemon>) {
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                // Out of descriptors or an aborted client: both pass.
                Err(error) => {
                    eprintln!("slopwatchd: accept failed: {error}");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
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
