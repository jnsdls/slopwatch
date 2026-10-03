//! Stamps `SLOPWATCH_BUILD_ID`: the git SHA, plus a hash of the uncommitted
//! changes when the tree is dirty, so two builds of different trees never
//! share an id.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    for path in watched_paths(&workspace) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SLOPWATCH_BUILD_ID");

    let id = std::env::var("SLOPWATCH_BUILD_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| build_id(&workspace))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SLOPWATCH_BUILD_ID={id}");
}

fn build_id(workspace: &Path) -> Option<String> {
    let sha = git(workspace, &["rev-parse", "--short=12", "HEAD"])?;
    let changes = git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if changes.is_empty() {
        return Some(sha);
    }
    Some(format!("{sha}+dirty.{}", dirty_hash(workspace, &changes)?))
}

/// Hashes the diff against HEAD and the contents of untracked files with
/// `git hash-object`, which keeps the build script free of dependencies.
fn dirty_hash(workspace: &Path, status: &str) -> Option<String> {
    let mut input = git(workspace, &["diff", "HEAD", "--binary"])?.into_bytes();
    let untracked = status.lines().filter_map(|line| line.strip_prefix("?? "));
    for path in untracked {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
        input.extend(std::fs::read(workspace.join(path)).unwrap_or_default());
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

/// Sources and git state that change the build id. Cargo only reruns the
/// script for these, so `target/` churn doesn't restamp every build.
fn watched_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        workspace.join("crates"),
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        workspace.join("rust-toolchain.toml"),
    ];
    for git_path in ["HEAD", "index", "logs/HEAD"] {
        if let Some(path) = git(workspace, &["rev-parse", "--git-path", git_path]) {
            paths.push(workspace.join(path));
        }
    }
    paths
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
