mod support;

use slopwatch_core::load;
use support::{TestResolver, load_ok};

fn library() -> TestResolver {
    TestResolver::default().with_library(
        "agent-review",
        "uses: claude\nwith:\n  model: sonnet\n  depth: 2\n",
    )
}

fn review_key(node: &str, resolver: &TestResolver) -> slopwatch_core::ReuseKey {
    let text = format!("version: 1\nsteps:\n  review: {node}\ngate: [review]\n");
    let pipeline = load(&text, resolver).unwrap();
    pipeline
        .step("review")
        .unwrap()
        .reuse_key("abc123", "1.0.0+deadbeef")
}

#[test]
fn a_with_override_changes_the_config_hash_and_so_the_reuse_key() {
    let resolver = library();
    let plain = review_key("{ uses: lib/agent-review }", &resolver);
    let overridden = review_key(
        "{ uses: lib/agent-review, with: { model: opus } }",
        &resolver,
    );
    assert_ne!(plain.config_hash, overridden.config_hash);
    assert_ne!(plain, overridden);
}

#[test]
fn an_override_that_restates_the_library_value_keeps_the_key() {
    let resolver = library();
    let plain = review_key("{ uses: lib/agent-review }", &resolver);
    let restated = review_key(
        "{ uses: lib/agent-review, with: { depth: 2, model: sonnet } }",
        &resolver,
    );
    assert_eq!(plain, restated);
}

#[test]
fn an_edited_library_step_changes_the_key() {
    let before = review_key("{ uses: lib/agent-review }", &library());
    let edited = TestResolver::default().with_library(
        "agent-review",
        "uses: claude\nwith:\n  model: haiku\n  depth: 2\n",
    );
    let after = review_key("{ uses: lib/agent-review }", &edited);
    assert_ne!(before, after);
}

#[test]
fn the_key_holds_head_sha_step_and_plugin_version() {
    let pipeline = load_ok("version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n");
    let ci = pipeline.step("ci").unwrap();
    let key = ci.reuse_key("abc123", "1.0.0+deadbeef");
    assert_eq!(key.head_sha, "abc123");
    assert_eq!(key.step, "ci");
    assert_eq!(key.plugin_version, "1.0.0+deadbeef");
    assert_eq!(key.config_hash.len(), 64);
    assert_ne!(key, ci.reuse_key("abc124", "1.0.0+deadbeef"));
    assert_ne!(key, ci.reuse_key("abc123", "1.0.1+deadbeef"));
}

#[test]
fn key_order_in_the_file_does_not_change_the_hash() {
    let a = load_ok(
        "version: 1\nsteps:\n  r: { uses: claude, with: { a: 1, b: { x: 1, y: 2 } } }\ngate: [r]\n",
    );
    let b = load_ok(
        "version: 1\nsteps:\n  r: { uses: claude, with: { b: { y: 2, x: 1 }, a: 1 } }\ngate: [r]\n",
    );
    assert_eq!(
        a.step("r").unwrap().config_hash(),
        b.step("r").unwrap().config_hash()
    );
}

#[test]
fn only_verdicts_a_step_reported_are_reused() {
    use slopwatch_core::Verdict;

    for verdict in [Verdict::Pass, Verdict::Fail, Verdict::Inconclusive] {
        assert!(verdict.reusable(), "{verdict}");
    }
    // A skip depends on the rest of the Run, so the new Run decides it again.
    for verdict in [
        Verdict::Error,
        Verdict::Cancelled,
        Verdict::Missing,
        Verdict::Skipped,
    ] {
        assert!(!verdict.reusable(), "{verdict}");
    }
}
