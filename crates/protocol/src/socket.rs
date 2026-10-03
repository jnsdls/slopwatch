use std::path::PathBuf;

/// Overrides the socket path, for tests and for running a second daemon by
/// hand next to the installed one.
pub const SOCKET_ENV: &str = "SLOPWATCH_SOCKET";

/// The URL clients put in the WebSocket handshake. A Unix socket has no
/// host, so the host here is a placeholder the daemon ignores.
pub const LOCAL_URL: &str = "ws://localhost/";

/// Where the daemon listens and clients connect:
/// `~/Library/Application Support/slopwatch/daemon.sock`, unless
/// `SLOPWATCH_SOCKET` names another path.
pub fn local_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os(SOCKET_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/slopwatch/daemon.sock")
}
