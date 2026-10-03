//! The `slopwatchd` binary itself, pointed at a temp data dir.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slopwatch_protocol::{DATA_DIR_ENV, socket_path};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn slopwatchd(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_slopwatchd"));
    command
        .env(DATA_DIR_ENV, data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_second_daemon_on_the_same_data_dir_exits() {
    let dir = tempfile::tempdir().unwrap();
    let _first = Running(slopwatchd(dir.path()).spawn().unwrap());
    wait_for("the first daemon's socket", || {
        socket_path(dir.path()).exists()
    });

    let second = slopwatchd(dir.path()).output().unwrap();

    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("another daemon holds"), "{stderr}");
}
