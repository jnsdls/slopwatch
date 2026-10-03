//! Where Secret values live: the developer's login Keychain, one generic
//! password item per Secret, with the flavor's service name and the
//! Secret's name as the account.
//!
//! The daemon writes and reads its items through `/usr/bin/security`, not
//! the Security framework. The Keychain ties an item's access to the code
//! that created it, and an ad-hoc signed daemon is new code on every
//! rebuild (ADR 0009, "What the first build showed"), so items the daemon
//! made itself would prompt after each update. `/usr/bin/security` is
//! Apple-signed and the same on every build, and its items name it as
//! their trusted app (`-T`), so a rebuilt daemon reads them with no prompt.
//! The value goes in on stdin as hex, never in argv where `ps` shows it,
//! and comes back on a pipe (ADR 0014).

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use slopwatch_protocol::{Flavor, SecretValue};

/// Overrides the Keychain service name, for tests and for a second daemon
/// run by hand.
pub const SERVICE_ENV: &str = "SLOPWATCH_KEYCHAIN_SERVICE";

/// Keeps Secret values. Implementations never put a value in an error.
pub trait Keychain: Send + Sync {
    /// Creates or replaces the item for `name`.
    fn set(&self, name: &str, value: &SecretValue) -> Result<(), KeychainError>;
    /// The value for `name`, or `None` when there's no item.
    fn get(&self, name: &str) -> Result<Option<SecretValue>, KeychainError>;
    /// Removes the item for `name`. A missing item is fine.
    fn delete(&self, name: &str) -> Result<(), KeychainError>;
}

/// Why the Keychain couldn't do something. The message names the Secret,
/// never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainError(pub String);

impl fmt::Display for KeychainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The login Keychain, through `/usr/bin/security`.
pub struct SecurityCli {
    service: String,
    program: PathBuf,
    /// How long one `security` call may take. A locked Keychain makes it
    /// wait for the developer's password, and the daemon gives up rather
    /// than hang.
    timeout: Duration,
}

/// `security`'s exit status when no item matches.
const NOT_FOUND: i32 = 44;

impl SecurityCli {
    /// Items under `service`. Anything but letters, digits, `.`, `_` and
    /// `-` in it becomes `-`, since it goes into a `security -i` command
    /// line.
    pub fn new(service: impl Into<String>) -> Self {
        let service: String = service
            .into()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        Self {
            service,
            program: PathBuf::from("/usr/bin/security"),
            timeout: Duration::from_secs(30),
        }
    }

    /// `$SLOPWATCH_KEYCHAIN_SERVICE`, or else `slopwatch` for release
    /// builds and `slopwatch-dev` for dev builds, so a dev build never
    /// touches the real Secrets.
    pub fn default_service(flavor: Flavor) -> String {
        std::env::var(SERVICE_ENV)
            .ok()
            .filter(|service| !service.is_empty())
            .unwrap_or_else(|| {
                match flavor {
                    Flavor::Release => "slopwatch",
                    Flavor::Dev => "slopwatch-dev",
                }
                .to_owned()
            })
    }

    fn run(&self, args: &[&str], stdin: Option<String>) -> Result<Output, KeychainError> {
        let mut child = Command::new(&self.program)
            .args(args)
            .env_clear()
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| KeychainError(format!("can't run security: {error}")))?;
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            // A write error shows up as the exit status below.
            let _ = pipe.write_all(text.as_bytes());
        }
        let pid = child.id() as libc::pid_t;
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(child.wait_with_output());
        });
        match finished.recv_timeout(self.timeout) {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err(KeychainError(format!("security failed: {error}"))),
            Err(_) => {
                // SAFETY: kill takes plain integers. The child isn't
                // reaped until the waiting thread returns, so the pid is
                // still ours.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                Err(KeychainError(format!(
                    "the Keychain didn't answer within {}s. Is it locked?",
                    self.timeout.as_secs()
                )))
            }
        }
    }
}

/// What `security` said on stderr, for an error message. It never echoes
/// a value, but `secret` is cut out anyway in case a later one does.
fn complaint(output: &Output, secret: Option<&str>) -> String {
    let mut text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "***");
    }
    match output.status.code() {
        Some(code) if text.is_empty() => format!("security exited with status {code}"),
        _ if text.is_empty() => "security was killed".to_owned(),
        _ => text,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

impl Keychain for SecurityCli {
    fn set(&self, name: &str, value: &SecretValue) -> Result<(), KeychainError> {
        let encoded = hex(value.expose().as_bytes());
        // `security -i` reads commands from stdin, so the value stays out
        // of argv. Names and the service are plain words, checked before
        // they get here.
        let command = format!(
            "add-generic-password -U -a {name} -s {} -l {} -T {} -X {encoded}\n",
            self.service,
            self.service,
            self.program.display(),
        );
        let output = self.run(&["-i"], Some(command))?;
        // `security -i` exits 0 even when a command fails, and reports the
        // failure on stderr.
        if !output.status.success() || !output.stderr.is_empty() {
            let text = complaint(&output, Some(&encoded));
            return Err(KeychainError(format!(
                "can't set `{name}` in the Keychain: {}",
                text.replace(value.expose(), "***")
            )));
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Option<SecretValue>, KeychainError> {
        let output = self.run(
            &[
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                name,
                "-w",
            ],
            None,
        )?;
        if output.status.code() == Some(NOT_FOUND) {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(KeychainError(format!(
                "can't read `{name}` from the Keychain: {}",
                complaint(&output, None)
            )));
        }
        let mut value = String::from_utf8(output.stdout)
            .map_err(|_| KeychainError(format!("`{name}` in the Keychain isn't text")))?;
        if value.ends_with('\n') {
            value.pop();
        }
        Ok(Some(SecretValue::new(value)))
    }

    fn delete(&self, name: &str) -> Result<(), KeychainError> {
        let output = self.run(
            &["delete-generic-password", "-s", &self.service, "-a", name],
            None,
        )?;
        if output.status.success() || output.status.code() == Some(NOT_FOUND) {
            return Ok(());
        }
        Err(KeychainError(format!(
            "can't remove `{name}` from the Keychain: {}",
            complaint(&output, None)
        )))
    }
}

/// A Keychain in memory, for tests and for daemons that shouldn't touch
/// the developer's.
#[derive(Default)]
pub struct MemoryKeychain {
    items: Mutex<HashMap<String, SecretValue>>,
    /// How many times each name was read, so tests can check the cache.
    reads: Mutex<HashMap<String, usize>>,
}

impl MemoryKeychain {
    pub fn reads(&self, name: &str) -> usize {
        self.reads
            .lock()
            .expect("no panics while holding reads")
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    fn items(&self) -> std::sync::MutexGuard<'_, HashMap<String, SecretValue>> {
        self.items
            .lock()
            .expect("no panics while holding the items")
    }
}

impl Keychain for MemoryKeychain {
    fn set(&self, name: &str, value: &SecretValue) -> Result<(), KeychainError> {
        self.items().insert(name.to_owned(), value.clone());
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Option<SecretValue>, KeychainError> {
        *self
            .reads
            .lock()
            .expect("no panics while holding reads")
            .entry(name.to_owned())
            .or_default() += 1;
        Ok(self.items().get(name).cloned())
    }

    fn delete(&self, name: &str) -> Result<(), KeychainError> {
        self.items().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_go_to_security_as_hex() {
        assert_eq!(
            hex("a \"b\"\n".as_bytes()),
            "6120226222 0a".replace(' ', "")
        );
    }

    /// Against the real login Keychain, under a service only this test
    /// uses, deleting what it made. Run by hand:
    /// `cargo test -p slopwatch-daemon keychain -- --ignored`.
    #[test]
    #[ignore = "touches the developer's login Keychain"]
    fn the_login_keychain_sets_rotates_reads_and_deletes_without_a_prompt() {
        let service = format!("slopwatch-test-70-{}", std::process::id());
        let keychain = SecurityCli::new(&service);
        let name = "SLOPWATCH_TEST_70";
        let value = SecretValue::new("first value with \"quotes\", \\ and ;|$(x)");

        assert_eq!(keychain.get(name).unwrap(), None);
        keychain.set(name, &value).unwrap();
        let read = keychain.get(name).unwrap();
        let rotated = SecretValue::new("second-value-0123456789");
        keychain.set(name, &rotated).unwrap();
        let read_rotated = keychain.get(name).unwrap();
        keychain.delete(name).unwrap();
        let gone = keychain.get(name).unwrap();
        keychain.delete(name).unwrap();

        // Compared without printing either value.
        assert!(read == Some(value), "the first value didn't read back");
        assert!(
            read_rotated == Some(rotated),
            "the rotated value didn't read back"
        );
        assert!(gone.is_none(), "the item is still there");
    }

    /// Whether an item one build wrote reads back from a different build
    /// with no prompt (ADR 0014). Run by hand in three steps, rebuilding in
    /// between with a different `SLOPWATCH_TEST_70_BUILD`, which lands in
    /// the binary and so changes its ad-hoc signature:
    ///
    /// ```text
    /// SLOPWATCH_TEST_70_BUILD=a SLOPWATCH_TEST_70_PHASE=write cargo test -p slopwatch-daemon across_a_rebuild -- --ignored
    /// SLOPWATCH_TEST_70_BUILD=b SLOPWATCH_TEST_70_PHASE=read cargo test -p slopwatch-daemon across_a_rebuild -- --ignored
    /// ```
    ///
    /// The read phase deletes the item.
    #[test]
    #[ignore = "touches the developer's login Keychain"]
    fn an_item_reads_back_across_a_rebuild() {
        const BUILD: Option<&str> = option_env!("SLOPWATCH_TEST_70_BUILD");
        let keychain = SecurityCli::new("slopwatch-test-70-rebuild");
        let name = "SLOPWATCH_TEST_70";
        let value = SecretValue::new("rebuild-value-0123456789");
        eprintln!("build {BUILD:?}");
        match std::env::var("SLOPWATCH_TEST_70_PHASE").as_deref() {
            Ok("write") => keychain.set(name, &value).unwrap(),
            Ok("read") => {
                let read = keychain.get(name).unwrap();
                keychain.delete(name).unwrap();
                assert!(read == Some(value), "the value didn't read back");
            }
            _ => {}
        }
    }
}
