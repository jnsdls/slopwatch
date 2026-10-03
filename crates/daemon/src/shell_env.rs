//! The `PATH` Steps get. launchd starts the daemon with a bare `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), so CLIs the developer's shell finds,
//! such as `claude`, `gh` or `node` under nvm, Homebrew or mise, would go
//! missing. The daemon asks the developer's login shell once, at start, and
//! merges the `PATH` it reports after its own.
//!
//! Borrowed from zeron's `shell_env` and `compose_child_path` (MIT): run
//! the shell interactive and login first, since nvm and friends load from
//! rc files, and fall back to login only, for rc files that hang or `exec`
//! a multiplexer when interactive. The shell prints its env between markers,
//! and the reader stops at the end marker, so init that blocks after
//! printing, or a grandchild holding the pipe, can't wedge the daemon. Each
//! attempt is killed after a few seconds.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BEGIN: &str = "__SLOPWATCH_SHELL_ENV_BEGIN__";
const END: &str = "__SLOPWATCH_SHELL_ENV_END__";
/// A runaway rc file can't make the daemon hold more than this.
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// After the shell exits, how long the pipe gets to flush.
const EXIT_GRACE: Duration = Duration::from_millis(250);

/// The `PATH` the developer's login shell reports, or `None` if no shell
/// answers. Blocks for up to two attempts.
pub fn login_shell_path() -> Option<String> {
    let shell = user_shell()?;
    snapshot_path(&shell, ATTEMPT_TIMEOUT)
}

/// The daemon's own `PATH` first, then the login shell's, each directory
/// once.
pub fn merge(own: Option<&str>, login: Option<&str>) -> String {
    let mut seen = HashSet::new();
    own.into_iter()
        .chain(login)
        .flat_map(|path| path.split(':'))
        .filter(|dir| !dir.is_empty() && seen.insert(*dir))
        .collect::<Vec<_>>()
        .join(":")
}

/// `$SHELL`, then the passwd entry, then the usual shells, skipping
/// anything that isn't an executable or is a nologin shell.
fn user_shell() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(shell) = std::env::var_os("SHELL").filter(|shell| !shell.is_empty()) {
        candidates.push(shell.into());
    }
    // launchd starts agents without SHELL; passwd still has it.
    if let Some(shell) = passwd_shell() {
        candidates.push(shell);
    }
    candidates.extend(["/bin/zsh", "/bin/bash", "/bin/sh"].map(PathBuf::from));
    candidates.into_iter().find(|path| {
        let name = path.file_name().map(|name| name.to_string_lossy());
        !matches!(name.as_deref(), Some("nologin" | "false") | None) && is_executable(path)
    })
}

fn passwd_shell() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getpwuid returns a pointer into a static buffer or null. It's
    // read right away, and only the daemon's start calls this.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            return None;
        }
        let shell = std::ffi::CStr::from_ptr((*pw).pw_shell).to_bytes();
        (!shell.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(shell)))
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Flags to try, most loaded first. csh and tcsh refuse `-l` with `-c`,
/// and fish reads its config on every start, so `-l` alone loads it all.
fn attempts(shell: &Path) -> &'static [&'static [&'static str]] {
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("csh" | "tcsh") => &[&["-c"]],
        Some("fish") => &[&["-l", "-c"], &["-c"]],
        _ => &[&["-l", "-i", "-c"], &["-l", "-c"]],
    }
}

fn snapshot_path(shell: &Path, timeout: Duration) -> Option<String> {
    let script = format!("echo {BEGIN}; env; echo {END}");
    attempts(shell)
        .iter()
        .find_map(|flags| parse_path(&capture(shell, flags, &script, timeout)))
}

/// Runs the shell and collects its stdout until the end marker shows up,
/// the shell exits and the grace runs out, or the timeout kills it. A
/// thread of its own reads the pipe, so nothing here waits on EOF.
fn capture(shell: &Path, flags: &[&str], script: &str, timeout: Duration) -> Vec<u8> {
    let child = Command::new(shell)
        .args(flags)
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Lets rc files skip work for this probe; a dumb terminal quiets
        // fancy prompts.
        .env("SLOPWATCH_RESOLVING_ENVIRONMENT", "1")
        .env("TERM", "dumb")
        .spawn();
    let Ok(mut child) = child else {
        return Vec::new();
    };
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    if let Some(mut stdout) = child.stdout.take() {
        let output = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = stdout.read(&mut chunk) {
                let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
                if output.len() >= MAX_OUTPUT {
                    break;
                }
                output.extend_from_slice(&chunk[..n]);
            }
        });
    }
    let deadline = Instant::now() + timeout;
    let mut exited: Option<Instant> = None;
    loop {
        if find(&output.lock().unwrap_or_else(|e| e.into_inner()), END).is_some() {
            break;
        }
        match exited {
            Some(at) if at.elapsed() >= EXIT_GRACE => break,
            Some(_) => {}
            None => match child.try_wait() {
                Ok(Some(_)) => exited = Some(Instant::now()),
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) | Err(_) => break,
            },
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if exited.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    output.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `PATH` from the env dump between the last begin marker and the end
/// marker after it. Noise from rc files, a stray marker included, lands
/// before the real dump.
fn parse_path(output: &[u8]) -> Option<String> {
    let begin = output
        .windows(BEGIN.len())
        .rposition(|window| window == BEGIN.as_bytes())?;
    let dump = &output[begin + BEGIN.len()..];
    let dump = &dump[..find(dump, END)?];
    String::from_utf8_lossy(dump).lines().find_map(|line| {
        line.strip_prefix("PATH=")
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
    })
}

fn find(haystack: &[u8], needle: &str) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake shell: sets up its env, skips its flags and runs the `-c`
    /// script.
    fn fake_shell(dir: &Path, setup: &str) -> PathBuf {
        let path = dir.join("fake-shell");
        let body = format!(
            "#!/bin/sh\n{setup}\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -c ]; then shift; exec /bin/sh -c \"$1\"; fi\n  shift\ndone\nexit 1\n"
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn merges_the_daemons_path_first_with_each_directory_once() {
        assert_eq!(
            merge(
                Some("/usr/bin:/bin"),
                Some("/opt/homebrew/bin:/usr/bin::/Users/me/.local/bin")
            ),
            "/usr/bin:/bin:/opt/homebrew/bin:/Users/me/.local/bin"
        );
        assert_eq!(merge(Some("/usr/bin"), None), "/usr/bin");
    }

    #[test]
    fn reads_path_between_the_last_markers() {
        let output = format!(
            "{BEGIN}\nnoise\n{BEGIN}\nHOME=/h\nPATH=/real/bin:/usr/bin\n{END}\nPATH=/late\n"
        );

        assert_eq!(
            parse_path(output.as_bytes()).as_deref(),
            Some("/real/bin:/usr/bin")
        );
    }

    #[test]
    fn asks_the_shell_for_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(dir.path(), "PATH=/from/login:/usr/bin:/bin; export PATH");

        let path = snapshot_path(&shell, Duration::from_secs(10)).unwrap();

        assert!(path.starts_with("/from/login:"), "{path}");
    }

    #[test]
    fn falls_back_to_a_login_shell_when_the_interactive_one_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(
            dir.path(),
            "case \" $* \" in *\" -i \"*) exec sleep 60;; esac\nPATH=/fallback:/usr/bin:/bin; export PATH",
        );
        let started = Instant::now();

        // Each attempt gets 2 s, so the login-only one answers in time even
        // on a loaded machine, and the hung one is killed long before its
        // 60 s are up.
        let path = snapshot_path(&shell, Duration::from_secs(2)).unwrap();

        assert!(path.starts_with("/fallback:"), "{path}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn gives_up_on_a_shell_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        // exec, so killing the shell kills the sleep too.
        let shell = fake_shell(dir.path(), "exec sleep 60");

        assert_eq!(snapshot_path(&shell, Duration::from_millis(300)), None);
    }
}
