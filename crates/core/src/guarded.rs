//! Guarded paths (ADR 0008): files no Step may change through slopwatch,
//! so that no Step can pass the Gate by weakening what judges it. CI
//! workflows, other CI configs and the Pipeline file are always guarded.
//! Lockfiles are guarded unless the Pipeline sets `guard_lockfiles: false`.

/// What a Pipeline guards besides what's always guarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    pub lockfiles: bool,
}

impl Default for Guard {
    fn default() -> Self {
        Self { lockfiles: true }
    }
}

/// Directories whose every file is guarded, as a path starts.
const GUARDED_DIRS: &[&str] = &[
    ".github/",
    ".slopwatch/",
    ".circleci/",
    ".buildkite/",
    ".woodpecker/",
    ".gitlab/",
];

/// CI config files at the repo root.
const GUARDED_FILES: &[&str] = &[
    ".gitlab-ci.yml",
    ".travis.yml",
    ".drone.yml",
    ".woodpecker.yml",
    ".appveyor.yml",
    "appveyor.yml",
    "azure-pipelines.yml",
    "bitbucket-pipelines.yml",
    "jenkinsfile",
    "cloudbuild.yaml",
    "cloudbuild.yml",
];

/// Lockfile names, anywhere in the tree.
const LOCKFILES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "deno.lock",
    "gemfile.lock",
    "poetry.lock",
    "pipfile.lock",
    "pdm.lock",
    "uv.lock",
    "composer.lock",
    "go.sum",
    "package.resolved",
    "podfile.lock",
    "mix.lock",
    "pubspec.lock",
    "flake.lock",
    "gradle.lockfile",
    "packages.lock.json",
];

/// Why `path`, relative to the repo root, is guarded under `guard`, or
/// `None` if a Step may change it. Case is ignored, so a case-insensitive
/// checkout can't sneak a workflow in under another spelling.
pub fn guarded(path: &str, guard: Guard) -> Option<&'static str> {
    let path = path.trim_start_matches("./").to_ascii_lowercase();
    if GUARDED_DIRS.iter().any(|dir| path.starts_with(dir)) || GUARDED_FILES.contains(&&*path) {
        return Some("CI config or Pipeline");
    }
    let name = path.rsplit('/').next().unwrap_or(&path);
    (guard.lockfiles && LOCKFILES.contains(&name)).then_some("lockfile")
}
