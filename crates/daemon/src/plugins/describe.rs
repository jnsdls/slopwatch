//! Asking a third-party Plugin for its manifest before anyone approved it
//! (ADR 0012).
//!
//! The daemon runs `<exe> describe` locked down: no Secrets and a
//! scrubbed env (only `PATH`, with `HOME` and `TMPDIR` pointing at an
//! empty temporary directory that is also its working directory), stdin
//! closed, and a 5 s timeout. It runs in its own process group, and the
//! whole group is killed once `describe` is done or out of time, so
//! nothing it started outlives it. A child that leaves the group with
//! `setsid` escapes that, as it would any process-group kill; without an
//! OS sandbox (ADR 0003) the daemon can't do more.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use slopwatch_core::{BUILTIN_PLUGINS, parse_duration};
use slopwatch_protocol::is_secret_name;
use slopwatch_protocol::step::{Manifest, STEP_DIALECT};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// How long `describe` may take before it's killed.
pub const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most stdout a manifest may take.
const MANIFEST_LIMIT: u64 = 1 << 20;
/// How much of `describe`'s stderr a failure quotes.
const STDERR_LIMIT: u64 = 4 << 10;

/// Runs `program describe` locked down with `path` as its `PATH`, and
/// returns the manifest it printed, or why it couldn't.
pub async fn describe(program: &Path, path: &str, timeout: Duration) -> Result<Manifest, String> {
    let home = tempfile::tempdir().map_err(|error| format!("can't make a temp dir: {error}"))?;
    let mut command = tokio::process::Command::new(program);
    command
        .arg("describe")
        .current_dir(home.path())
        .env_clear()
        .env("PATH", path)
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("can't run `describe`: {error}"))?;
    let group = child.id().map(|pid| pid as libc::pid_t);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let finished = tokio::time::timeout(timeout, async {
        let (out, err, status) = tokio::join!(
            read_up_to(stdout, MANIFEST_LIMIT + 1),
            read_up_to(stderr, STDERR_LIMIT),
            child.wait(),
        );
        (out, err, status)
    })
    .await;
    if let Some(group) = group {
        // SAFETY: killpg takes plain integers. The group is the one this
        // spawn made, and a group that's already gone returns ESRCH.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let Ok((out, err, status)) = finished else {
        return Err(format!(
            "`describe` didn't finish within {timeout:?}, or left something holding its \
             output, and was killed"
        ));
    };
    let status = status.map_err(|error| format!("can't wait for `describe`: {error}"))?;
    let (out, err) = (out?, err?);
    if !status.success() {
        let said = String::from_utf8_lossy(&err);
        let said = said.trim();
        return Err(if said.is_empty() {
            format!("`describe` failed ({status})")
        } else {
            format!("`describe` failed ({status}): {said}")
        });
    }
    if out.len() as u64 > MANIFEST_LIMIT {
        return Err(format!(
            "`describe` printed more than {} KB",
            MANIFEST_LIMIT >> 10
        ));
    }
    serde_json::from_slice(&out)
        .map_err(|error| format!("`describe` didn't print a manifest: {error}"))
}

async fn read_up_to(stream: impl AsyncRead + Unpin, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    stream
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("can't read `describe`'s output: {error}"))?;
    Ok(bytes)
}

/// Why a file in the Plugins folder named `name` can't be a Plugin before
/// it even runs, if it can't.
pub fn check_name(name: &str) -> Option<String> {
    if BUILTIN_PLUGINS.contains(&name) {
        return Some(format!(
            "`{name}` is reserved for a built-in Plugin; rename the file"
        ));
    }
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    (!plain).then(|| {
        format!("`{name}` can't name a Plugin; use lowercase letters, digits, `-` and `_`")
    })
}

/// Why the manifest the Plugin file `name` described can't load, if it
/// can't.
pub fn check_manifest(name: &str, manifest: &Manifest) -> Option<String> {
    if manifest.id != name {
        return Some(format!(
            "its manifest names it `{}`, but the file is `{name}`",
            manifest.id
        ));
    }
    if manifest.dialect != STEP_DIALECT {
        return Some(format!(
            "it speaks Step dialect {}, and the daemon speaks {STEP_DIALECT}",
            manifest.dialect
        ));
    }
    if let Some(spec) = manifest
        .secrets
        .iter()
        .find(|spec| !is_secret_name(&spec.name))
    {
        return Some(format!(
            "it asks for a Secret named `{}`, which can't be a Secret",
            spec.name
        ));
    }
    for (key, value) in [
        ("timeout", &manifest.timeout),
        ("stall_after", &manifest.stall_after),
    ] {
        if let Some(value) = value
            && parse_duration(value).is_none()
        {
            return Some(format!(
                "its manifest sets `{key}: {value}`, which isn't a duration"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant;

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("plugin");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const MANIFEST: &str =
        r#"{"id":"lint","version":"1","dialect":1,"workspace":"read","secrets":["LINT_KEY"]}"#;

    #[tokio::test]
    async fn describe_prints_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), &format!("echo '{MANIFEST}'"));

        let manifest = describe(&program, "/usr/bin:/bin", DESCRIBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(manifest.id, "lint");
        assert_eq!(check_manifest("lint", &manifest), None);
    }

    #[tokio::test]
    async fn describe_gets_no_env_but_path_and_an_empty_home_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let program = script(
            dir.path(),
            &format!(
                "{{ env; echo \"cwd=$(pwd -P)\"; echo \"files=$(ls -A)\"; }} > '{}'; echo '{MANIFEST}'",
                seen.display()
            ),
        );

        describe(&program, "/usr/bin:/bin", DESCRIBE_TIMEOUT)
            .await
            .unwrap();
        let seen = std::fs::read_to_string(seen).unwrap();
        let vars: Vec<&str> = seen
            .lines()
            .filter(|line| !line.starts_with("cwd=") && !line.starts_with("files="))
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        // Nothing from the test's own env, such as `USER` or `CARGO_*`,
        // gets through. The shell adds PWD, SHLVL and `_` of its own.
        for var in &vars {
            assert!(
                ["PATH", "HOME", "TMPDIR", "PWD", "SHLVL", "_", "OLDPWD"].contains(var),
                "describe saw {var}: {seen}"
            );
        }
        assert!(seen.contains("PATH=/usr/bin:/bin\n"), "{seen}");
        let home = seen
            .lines()
            .find_map(|line| line.strip_prefix("HOME="))
            .unwrap();
        let cwd = seen
            .lines()
            .find_map(|line| line.strip_prefix("cwd="))
            .unwrap();
        // `pwd -P` resolves /var to /private/var, so compare the temp
        // dir's own name.
        assert_eq!(Path::new(home).file_name(), Path::new(cwd).file_name());
        assert!(seen.contains("files=\n"), "the dir starts empty: {seen}");
        assert!(!Path::new(home).exists(), "the temp dir goes afterwards");
    }

    #[tokio::test]
    async fn a_describe_that_hangs_is_killed_with_everything_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let program = script(
            dir.path(),
            &format!("sleep 60 & echo $! > '{}'; wait", pid.display()),
        );

        let started = Instant::now();
        let error = describe(&program, "/usr/bin:/bin", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(error.contains("didn't finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4));

        let child: i32 = std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: kill with signal 0 only checks the pid.
        while unsafe { libc::kill(child, 0) } == 0 {
            assert!(Instant::now() < deadline, "the sleep outlived describe");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[tokio::test]
    async fn the_real_timeout_is_five_seconds() {
        assert_eq!(DESCRIBE_TIMEOUT, Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        let error = describe(&program, "/usr/bin:/bin", DESCRIBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.contains("within 5s"), "{error}");
        let took = started.elapsed();
        assert!(
            took >= Duration::from_secs(5) && took < Duration::from_secs(8),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_or_garbled_describe_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let failing = script(dir.path(), "echo 'no config' >&2; exit 3");
        let error = describe(&failing, "/usr/bin:/bin", DESCRIBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.contains("no config"), "{error}");

        let garbled = script(dir.path(), "echo hello");
        let error = describe(&garbled, "/usr/bin:/bin", DESCRIBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.contains("didn't print a manifest"), "{error}");
    }

    #[test]
    fn reserved_and_odd_names_fail_before_anything_runs() {
        assert!(check_name("merge").unwrap().contains("reserved"));
        assert!(check_name("jev").unwrap().contains("reserved"));
        assert!(check_name("Lint").is_some());
        assert!(check_name("my lint").is_some());
        assert_eq!(check_name("my-lint_2"), None);
    }

    #[test]
    fn a_manifest_must_name_its_file_speak_the_dialect_and_ask_for_real_secrets() {
        let manifest: Manifest = serde_json::from_str(MANIFEST).unwrap();
        assert!(
            check_manifest("other", &manifest)
                .unwrap()
                .contains("`lint`")
        );

        let mut old = manifest.clone();
        old.dialect = 9;
        assert!(check_manifest("lint", &old).unwrap().contains("dialect 9"));

        let mut bad_secret = manifest.clone();
        bad_secret.secrets = vec![slopwatch_protocol::step::SecretSpec::required("PATH")];
        assert!(
            check_manifest("lint", &bad_secret)
                .unwrap()
                .contains("`PATH`")
        );

        let mut bad_timeout = manifest;
        bad_timeout.timeout = Some("soon".into());
        assert!(
            check_manifest("lint", &bad_timeout)
                .unwrap()
                .contains("soon")
        );
    }
}
