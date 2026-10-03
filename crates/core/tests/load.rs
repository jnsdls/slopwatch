mod support;

use std::time::Duration;

use slopwatch_core::{Uses, Workspace, load};
use support::{TestResolver, load_err, load_err_with, load_ok};

/// The shape from the Pipeline file format ticket, with Library Steps
/// swapped for Plugins the test resolver knows.
const EXAMPLE: &str = r#"
version: 1
steps:
  ci:       { uses: ci }
  desc:     { uses: jev }
  review:   { uses: claude, needs: [ci], with: { model: opus } }
  docs:     { uses: jev, when: { files: "docs/**" } }
  human:    { uses: human, needs: [review], with: { prompt: "Ship it?" } }
  merge:    { uses: merge, needs: [gate], when: gate }
  fix:      { uses: fix, needs: [gate] }
gate:
  - ci
  - desc
  - human
  - docs: [pass, skipped]
"#;

#[test]
fn loads_the_reference_pipeline() {
    let pipeline = load_ok(EXAMPLE);
    assert_eq!(pipeline.steps().count(), 7);
    assert_eq!(pipeline.gate_terms().len(), 4);
    assert_eq!(pipeline.fix_rounds(), 3);
    let fix = pipeline.step("fix").unwrap();
    assert!(fix.is_write());
    assert_eq!(fix.condition().to_string(), "{not: [gate]}");
    assert_eq!(
        pipeline.step("review").unwrap().condition().to_string(),
        "[ci]"
    );
    assert_eq!(
        pipeline.step("merge").unwrap().condition().to_string(),
        "gate"
    );
}

#[test]
fn rejects_an_unknown_plugin() {
    let errors = load_err("version: 1\nsteps:\n  a: { uses: nothing }\ngate: [a]\n");
    assert_eq!(
        errors,
        ["Step `a` uses Plugin `nothing`, which isn't installed"]
    );
}

#[test]
fn rejects_a_cycle() {
    let errors = load_err(
        r#"
version: 1
steps:
  a: { uses: ci, needs: [c] }
  b: { uses: jev, needs: [a] }
  c: { uses: jev, needs: [b] }
gate: [a]
"#,
    );
    assert_eq!(errors, ["the Pipeline has a cycle: a -> c -> b -> a"]);
}

#[test]
fn rejects_a_step_that_needs_itself() {
    let errors = load_err("version: 1\nsteps:\n  a: { uses: ci, needs: [a] }\ngate: [a]\n");
    assert_eq!(errors, ["the Pipeline has a cycle: a -> a"]);
}

#[test]
fn rejects_a_gate_that_reads_a_step_downstream_of_it() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:    { uses: ci }
  after: { uses: jev, needs: [gate] }
gate: [ci, after]
"#,
    );
    assert_eq!(errors, ["the Pipeline has a cycle: after -> gate -> after"]);
}

#[test]
fn rejects_a_gate_that_references_a_write_step() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:  { uses: ci }
  fix: { uses: fix, needs: [ci] }
gate: [ci, fix]
"#,
    );
    assert_eq!(
        errors,
        ["the Gate references `fix`, which declares `workspace: write`"]
    );
}

#[test]
fn rejects_a_condition_that_references_a_write_step() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:   { uses: ci }
  fix:  { uses: fix, needs: [gate] }
  note: { uses: jev, needs: [gate], when: { not: [fix] } }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        ["the Condition of Step `note` references `fix`, which declares `workspace: write`"]
    );
}

#[test]
fn rejects_a_step_that_needs_a_write_step() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:    { uses: ci }
  fix:   { uses: fix, needs: [gate] }
  after: { uses: jev, needs: [fix], when: always }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        ["Step `after` needs `fix`, which declares `workspace: write` and so must be terminal"]
    );
}

#[test]
fn rejects_a_condition_that_references_a_step_not_upstream() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  lint:   { uses: ci }
  review: { uses: claude, needs: [ci], when: lint }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        [
            "Step `review` has a Condition that references `lint`, which isn't upstream of it; add it to `needs`"
        ]
    );
}

#[test]
fn a_condition_may_reference_a_transitive_upstream_step() {
    load_ok(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  review: { uses: claude, needs: [ci] }
  human:  { uses: human, needs: [review], when: [ci, review] }
gate: [human]
"#,
    );
}

#[test]
fn rejects_a_condition_that_reads_the_gate_without_needing_it() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:    { uses: ci }
  merge: { uses: merge, needs: [gate] }
  note:  { uses: jev, when: gate }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        [
            "Step `note` has a Condition that references `gate`, which isn't upstream of it; add it to `needs`"
        ]
    );
}

#[test]
fn rejects_a_merge_upstream_of_the_gate() {
    let errors = load_err(
        r#"
version: 1
steps:
  merge: { uses: merge }
  ci:    { uses: ci, needs: [merge] }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        ["Merge Step `merge` is upstream of the Gate, so it could merge before the Gate decides"]
    );
}

#[test]
fn rejects_a_merge_not_after_the_gate() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:    { uses: ci }
  merge: { uses: merge, needs: [ci] }
gate: [ci]
"#,
    );
    assert_eq!(
        errors,
        ["Merge Step `merge` must come after the Gate; add `gate` to its `needs`"]
    );
}

#[test]
fn rejects_a_third_party_plugin_with_a_reserved_name() {
    let resolver = TestResolver::default().with_third_party("ci", Workspace::None);
    let errors = load_err_with(
        "version: 1\nsteps:\n  checks: { uses: ci }\ngate: [checks]\n",
        &resolver,
    );
    assert_eq!(
        errors,
        [
            "Step `checks` uses a third-party Plugin named `ci`, a name reserved for a built-in Plugin"
        ]
    );
}

#[test]
fn accepts_a_third_party_plugin_with_its_own_name() {
    let resolver = TestResolver::default().with_third_party("lint", Workspace::Read);
    let pipeline = load(
        "version: 1\nsteps:\n  lint: { uses: lint }\ngate: [lint]\n",
        &resolver,
    )
    .unwrap();
    assert!(!pipeline.step("lint").unwrap().builtin);
}

#[test]
fn rejects_fix_rounds_above_the_ceiling() {
    let errors = load_err("version: 1\nfix_rounds: 11\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n");
    assert_eq!(errors, ["`fix_rounds` is 11, above its ceiling of 10"]);
    let pipeline = load_ok("version: 1\nfix_rounds: 10\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n");
    assert_eq!(pipeline.fix_rounds(), 10);
}

#[test]
fn rejects_unknown_ids_in_needs_gate_and_conditions() {
    let errors = load_err(
        r#"
version: 1
steps:
  ci:     { uses: ci, needs: [nope] }
  review: { uses: claude, when: { or: [missing] } }
gate: [ci, ghost]
"#,
    );
    assert_eq!(
        errors,
        [
            "Step `ci` needs `nope`, which isn't a Step",
            "the Condition of Step `review` references `missing`, which isn't a Step",
            "the Gate references `ghost`, which isn't a Step",
        ]
    );
}

#[test]
fn rejects_an_empty_gate() {
    let errors = load_err("version: 1\nsteps:\n  ci: { uses: ci }\n");
    assert_eq!(errors, ["the Gate references no Steps"]);
}

#[test]
fn rejects_a_reserved_step_id() {
    let errors =
        load_err("version: 1\nsteps:\n  gate: { uses: ci }\n  ci: { uses: ci }\ngate: [ci]\n");
    assert_eq!(
        errors,
        ["Step `gate` can't use that id, because the Pipeline language reserves it"]
    );
}

#[test]
fn rejects_an_unknown_key_so_a_typo_cannot_drop_a_condition() {
    let errors = load_err("version: 1\nsteps:\n  ci: { uses: ci, whne: gate }\ngate: [ci]\n");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("unknown field `whne`"), "{}", errors[0]);
}

#[test]
fn rejects_an_invalid_condition_naming_the_step() {
    let errors = load_err(
        "version: 1\nsteps:\n  ci: { uses: ci }\n  r: { uses: claude, needs: [ci], when: { ci: [error] } }\ngate: [ci]\n",
    );
    assert_eq!(
        errors,
        ["Step `r` has an invalid Condition: `ci` can accept only pass and skipped, not error"]
    );
}

#[test]
fn rejects_a_gate_term_that_reads_pr_facts() {
    let errors = load_err("version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci, { draft: false }]\n");
    assert_eq!(
        errors,
        ["the Gate is invalid: the Gate reads only Verdicts, so it can't read the PR fact `draft`"]
    );
}

#[test]
fn rejects_an_unsupported_version() {
    let errors = load_err("version: 2\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n");
    assert_eq!(
        errors,
        ["unsupported Pipeline version 2; this build reads version 1"]
    );
}

#[test]
fn rejects_a_bad_duration() {
    let errors = load_err("version: 1\nsteps:\n  ci: { uses: ci, timeout: soon }\ngate: [ci]\n");
    assert_eq!(
        errors,
        ["Step `ci` sets `timeout: soon`; write a duration such as 90s, 30m or 2h"]
    );
}

#[test]
fn resolves_a_library_step_with_key_by_key_overrides() {
    let resolver = TestResolver::default().with_library(
        "agent-review",
        "uses: claude\nwith:\n  model: sonnet\n  focus: [security, tests]\n  prompt: Review it\ntimeout: 30m\nstall_after: 5m\n",
    );
    let pipeline = load(
        r#"
version: 1
steps:
  review: { uses: lib/agent-review, with: { model: opus, focus: [docs] }, timeout: 45m }
gate: [review]
"#,
        &resolver,
    )
    .unwrap();
    let review = pipeline.step("review").unwrap();
    assert_eq!(review.uses, Uses::Library("agent-review".into()));
    assert_eq!(review.plugin, "claude");
    assert_eq!(review.workspace, Workspace::Read);
    assert_eq!(
        serde_json::Value::Object(review.config.clone()),
        serde_json::json!({ "model": "opus", "focus": ["docs"], "prompt": "Review it" })
    );
    assert_eq!(review.timeout, Some(Duration::from_secs(45 * 60)));
    assert_eq!(review.stall_after, Some(Duration::from_secs(5 * 60)));
}

#[test]
fn rejects_a_missing_library_step() {
    let errors = load_err("version: 1\nsteps:\n  r: { uses: lib/nope }\ngate: [r]\n");
    assert_eq!(
        errors,
        ["Step `r` uses Library Step `nope`, which doesn't exist"]
    );
}

#[test]
fn rejects_a_library_step_that_uses_another_library_step() {
    let resolver = TestResolver::default().with_library("outer", "uses: lib/inner\n");
    let errors = load_err_with(
        "version: 1\nsteps:\n  r: { uses: lib/outer }\ngate: [r]\n",
        &resolver,
    );
    assert_eq!(
        errors,
        [
            "Step `r` uses Library Step `outer`, which is invalid: it uses `lib/inner`, but a Library Step must use a Plugin"
        ]
    );
}

#[test]
fn rejects_needs_in_a_library_step() {
    let resolver = TestResolver::default().with_library("x", "uses: ci\nneeds: [a]\n");
    let errors = load_err_with(
        "version: 1\nsteps:\n  r: { uses: lib/x }\ngate: [r]\n",
        &resolver,
    );
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].starts_with("Step `r` uses Library Step `x`, which is invalid:")
            && errors[0].contains("unknown field `needs`"),
        "{}",
        errors[0]
    );
}
