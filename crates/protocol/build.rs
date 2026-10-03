//! Stamps `SLOPWATCH_FLAVOR`, passed through from the build environment
//! (see `Flavor`), and `SLOPWATCH_BUILD_ID`: the git SHA, plus a hash of the
//! uncommitted changes to the build inputs when they're dirty, so builds of
//! different sources get different ids.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The paths, relative to the workspace root, whose contents go into a
/// build. Edits elsewhere, such as docs, don't change the build id.
const INPUTS: [&str; 4] = ["crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

fn main() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    for path in watched_paths(&workspace) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SLOPWATCH_BUILD_ID");
    println!("cargo:rerun-if-env-changed=SLOPWATCH_FLAVOR");

    let flavor = std::env::var("SLOPWATCH_FLAVOR").unwrap_or_default();
    println!("cargo:rustc-env=SLOPWATCH_FLAVOR={flavor}");

    let id = std::env::var("SLOPWATCH_BUILD_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| build_id(&workspace))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SLOPWATCH_BUILD_ID={id}");
}

fn build_id(workspace: &Path) -> Option<String> {
    let sha = git(workspace, &["rev-parse", "--short=12", "HEAD"])?;
    let status = git_with_inputs(
        workspace,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    if status.is_empty() {
        return Some(sha);
    }
    Some(format!("{sha}+dirty.{}", dirty_hash(workspace, &status)?))
}

/// Hashes the diff of the inputs against HEAD and the contents of untracked
/// files with `git hash-object`, which keeps the build script free of
/// dependencies.
fn dirty_hash(workspace: &Path, status: &str) -> Option<String> {
    let mut input = git_with_inputs(workspace, &["diff", "HEAD", "--binary"])?.into_bytes();
    let untracked = status
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("?? "));
    for path in untracked {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
        input.extend(std::fs::read(workspace.join(path)).ok()?);
    }

    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    std::io::Write::write_all(&mut child.stdin.take()?, &input).ok()?;
    let output = child.wait_with_output().ok()?;
    let hash = String::from_utf8(output.stdout).ok()?;
    Some(hash.trim().chars().take(12).collect())
}

/// The inputs plus the git state that moves HEAD. Cargo reruns the script
/// only when one of these changes, so `target/` churn doesn't restamp every
/// build.
fn watched_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = INPUTS.iter().map(|input| workspace.join(input)).collect();
    for git_path in ["HEAD", "index", "logs/HEAD"] {
        if let Some(path) = git(workspace, &["rev-parse", "--git-path", git_path]) {
            paths.push(workspace.join(path));
        }
    }
    // A missing path makes cargo rerun the script on every build.
    paths.retain(|path| path.exists());
    paths
}

fn git_with_inputs(workspace: &Path, args: &[&str]) -> Option<String> {
    let mut args = args.to_vec();
    args.push("--");
    args.extend(INPUTS);
    git(workspace, &args)
}

fn git(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
