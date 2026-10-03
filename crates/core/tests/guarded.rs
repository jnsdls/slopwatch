//! Guarded paths (ADR 0008): files no Step may change through slopwatch.

mod support;

use slopwatch_core::{Guard, guarded};
use support::load_ok;

#[test]
fn workflows_ci_configs_and_the_pipeline_are_always_guarded() {
    for path in [
        ".github/workflows/ci.yml",
        ".github/CODEOWNERS",
        ".slopwatch/pipeline.yml",
        ".gitlab-ci.yml",
        ".circleci/config.yml",
        ".buildkite/pipeline.yml",
        "azure-pipelines.yml",
        "Jenkinsfile",
        ".travis.yml",
        "bitbucket-pipelines.yml",
        // Case doesn't get around it.
        ".GitHub/workflows/ci.yml",
    ] {
        assert_eq!(
            guarded(path, Guard::default()),
            Some("CI config or Pipeline"),
            "{path}"
        );
        assert!(
            guarded(path, Guard { lockfiles: false }).is_some(),
            "{path} stays guarded with lockfiles unguarded"
        );
    }
}

#[test]
fn lockfiles_anywhere_are_guarded_unless_the_pipeline_unguards_them() {
    for path in [
        "Cargo.lock",
        "crates/app/Cargo.lock",
        "package-lock.json",
        "web/yarn.lock",
        "pnpm-lock.yaml",
        "go.sum",
        "poetry.lock",
        "uv.lock",
        "Gemfile.lock",
    ] {
        assert_eq!(guarded(path, Guard::default()), Some("lockfile"), "{path}");
        assert_eq!(guarded(path, Guard { lockfiles: false }), None, "{path}");
    }
}

#[test]
fn ordinary_files_arent_guarded() {
    for path in [
        "src/main.rs",
        "github/notes.md",
        "docs/.github.md",
        "Cargo.toml",
        "lock.rs",
        "src/.slopwatch.rs",
    ] {
        assert_eq!(guarded(path, Guard::default()), None, "{path}");
    }
}

#[test]
fn a_pipeline_guards_lockfiles_unless_it_says_otherwise() {
    let base = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n";
    assert!(load_ok(base).guard().lockfiles);
    let unguarded = base.replace("version: 1", "version: 1\nguard_lockfiles: false");
    assert!(!load_ok(&unguarded).guard().lockfiles);
}
