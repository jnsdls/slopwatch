mod support;

use slopwatch_core::{Decision, GateState, PrFacts, RunState, SkipReason, StepState, Verdict};
use support::load_ok;

fn state(steps: &[(&str, StepState)]) -> RunState {
    RunState {
        steps: steps
            .iter()
            .map(|(id, state)| ((*id).to_owned(), *state))
            .collect(),
        ..RunState::default()
    }
}

fn settled(v: Verdict) -> StepState {
    StepState::Settled(v)
}

const PASS: StepState = StepState::Settled(Verdict::Pass);
const FAIL: StepState = StepState::Settled(Verdict::Fail);

#[test]
fn or_passes_as_soon_as_one_side_passes_and_fails_only_once_both_fail() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  a: { uses: ci }
  b: { uses: jev }
gate:
  - or: [a, b]
"#,
    );
    assert_eq!(pipeline.gate(&state(&[])), GateState::Pending);
    assert_eq!(
        pipeline.gate(&state(&[("a", PASS), ("b", StepState::Running)])),
        GateState::Pass
    );
    assert_eq!(
        pipeline.gate(&state(&[("a", FAIL), ("b", StepState::Running)])),
        GateState::Pending
    );
    assert_eq!(
        pipeline.gate(&state(&[("a", FAIL), ("b", FAIL)])),
        GateState::Fail
    );
}

#[test]
fn a_waiver_counts_a_settled_verdict_as_pass() {
    let pipeline = load_ok("version: 1\nsteps:\n  a: { uses: ci }\ngate: [a]\n");
    let mut run = state(&[("a", FAIL)]);
    assert_eq!(pipeline.gate(&run), GateState::Fail);
    run.waived.insert("a".into());
    assert_eq!(pipeline.gate(&run), GateState::Pass);
}

#[test]
fn steps_start_once_their_upstream_settles() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  desc:   { uses: jev }
  review: { uses: claude, needs: [ci] }
gate: [ci, desc, review]
"#,
    );
    let plan = pipeline.plan(&state(&[]));
    assert_eq!(plan.decision("ci"), Some(&Decision::Start));
    assert_eq!(plan.decision("desc"), Some(&Decision::Start));
    assert_eq!(plan.decision("review"), Some(&Decision::Wait));

    let plan = pipeline.plan(&state(&[("ci", PASS), ("desc", StepState::Running)]));
    assert_eq!(plan.decision("ci"), None, "settled Steps get no decision");
    assert_eq!(plan.decision("desc"), None, "running Steps get no decision");
    assert_eq!(plan.decision("review"), Some(&Decision::Start));
    assert_eq!(plan.gate, GateState::Pending);
}

#[test]
fn a_false_condition_skips_with_a_reason_and_dependents_follow_their_own_conditions() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:      { uses: ci }
  docs:    { uses: jev, needs: [ci], when: { files: "docs/**" } }
  strict:  { uses: claude, needs: [docs] }
  lenient: { uses: claude, needs: [docs], when: { or: [{ docs: [pass, skipped] }] } }
  notes:   { uses: human, needs: [docs], when: always }
gate:
  - ci
  - docs: [pass, skipped]
"#,
    );
    let mut run = state(&[("ci", PASS)]);
    run.pr = PrFacts {
        files: vec!["src/lib.rs".into()],
        ..PrFacts::default()
    };
    let plan = pipeline.plan(&run);
    assert_eq!(
        plan.decision("docs"),
        Some(&Decision::Skip(SkipReason::Condition(
            "{files: [docs/**]}".into()
        )))
    );
    assert_eq!(
        plan.decision("strict"),
        Some(&Decision::Skip(SkipReason::Upstream {
            step: "docs".into(),
            verdict: Verdict::Skipped
        }))
    );
    assert_eq!(plan.decision("lenient"), Some(&Decision::Start));
    assert_eq!(plan.decision("notes"), Some(&Decision::Start));
    assert_eq!(
        plan.gate,
        GateState::Pass,
        "the skip counts toward the Gate"
    );
}

#[test]
fn skip_reasons_read_well() {
    assert_eq!(
        SkipReason::Upstream {
            step: "ci".into(),
            verdict: Verdict::Fail
        }
        .to_string(),
        "upstream Step `ci` ended fail"
    );
    assert_eq!(
        SkipReason::Condition("{files: [docs/**]}".into()).to_string(),
        "its Condition `{files: [docs/**]}` is false"
    );
    assert_eq!(
        SkipReason::Gate(GateState::Pass).to_string(),
        "the Gate is pass"
    );
    assert_eq!(SkipReason::RoundCap.to_string(), "round cap");
}

#[test]
fn the_default_condition_skips_as_soon_as_one_upstream_fails() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  lint:   { uses: ci }
  review: { uses: claude, needs: [ci, lint] }
gate: [ci]
"#,
    );
    let plan = pipeline.plan(&state(&[("ci", FAIL), ("lint", StepState::Running)]));
    assert_eq!(
        plan.decision("review"),
        Some(&Decision::Skip(SkipReason::Upstream {
            step: "ci".into(),
            verdict: Verdict::Fail
        }))
    );
}

#[test]
fn an_explicit_true_condition_still_waits_for_upstream_to_settle() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:    { uses: ci }
  notes: { uses: jev, needs: [ci], when: always }
gate: [ci]
"#,
    );
    assert_eq!(
        pipeline.plan(&state(&[])).decision("notes"),
        Some(&Decision::Wait)
    );
    let plan = pipeline.plan(&state(&[("ci", FAIL)]));
    assert_eq!(plan.decision("notes"), Some(&Decision::Start));
}

const GATED: &str = r#"
version: 1
steps:
  ci:    { uses: ci }
  human: { uses: human, needs: [ci] }
  merge: { uses: merge, needs: [gate], when: gate }
  fix:   { uses: fix, needs: [gate] }
gate: [ci, human]
"#;

#[test]
fn merge_and_fix_wait_for_the_gate() {
    let pipeline = load_ok(GATED);
    let plan = pipeline.plan(&state(&[("ci", PASS), ("human", StepState::Running)]));
    assert_eq!(plan.gate, GateState::Pending);
    assert_eq!(plan.decision("merge"), Some(&Decision::Wait));
    assert_eq!(plan.decision("fix"), Some(&Decision::Wait));
}

#[test]
fn a_passing_gate_starts_merge_and_skips_fix() {
    let pipeline = load_ok(GATED);
    let plan = pipeline.plan(&state(&[("ci", PASS), ("human", PASS)]));
    assert_eq!(plan.gate, GateState::Pass);
    assert_eq!(plan.decision("merge"), Some(&Decision::Start));
    assert_eq!(
        plan.decision("fix"),
        Some(&Decision::Skip(SkipReason::Gate(GateState::Pass)))
    );
}

#[test]
fn a_failing_gate_starts_fix_and_skips_merge_without_waiting_on_the_rest() {
    let pipeline = load_ok(GATED);
    let plan = pipeline.plan(&state(&[("ci", FAIL)]));
    assert_eq!(plan.gate, GateState::Fail);
    assert_eq!(
        plan.decision("human"),
        Some(&Decision::Skip(SkipReason::Upstream {
            step: "ci".into(),
            verdict: Verdict::Fail
        }))
    );
    assert_eq!(
        plan.decision("merge"),
        Some(&Decision::Skip(SkipReason::Condition("gate".into())))
    );
    assert_eq!(plan.decision("fix"), Some(&Decision::Start));
}

#[test]
fn a_step_needing_the_gate_by_default_is_skipped_when_it_fails() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  notify: { uses: jev, needs: [gate] }
gate: [ci]
"#,
    );
    let plan = pipeline.plan(&state(&[("ci", FAIL)]));
    assert_eq!(
        plan.decision("notify"),
        Some(&Decision::Skip(SkipReason::Gate(GateState::Fail)))
    );
}

#[test]
fn the_round_cap_applies_only_once_a_write_step_would_run() {
    let pipeline = load_ok(&GATED.replace("version: 1", "version: 1\nfix_rounds: 1"));
    let mut run = state(&[("ci", PASS)]);
    run.fix_round = 1;
    assert_eq!(pipeline.plan(&run).decision("fix"), Some(&Decision::Wait));
    run.steps.insert("human".into(), PASS);
    assert_eq!(
        pipeline.plan(&run).decision("fix"),
        Some(&Decision::Skip(SkipReason::Gate(GateState::Pass))),
        "a shippable Run doesn't report the round cap"
    );
}

#[test]
fn gate_counts_the_skips_the_state_implies() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  a: { uses: ci }
  b: { uses: jev, needs: [a] }
gate:
  - b: [pass, skipped]
"#,
    );
    let run = state(&[("a", FAIL)]);
    assert_eq!(pipeline.gate(&run), GateState::Pass);
    assert_eq!(pipeline.plan(&run).gate, GateState::Pass);
}

#[test]
fn write_steps_are_skipped_once_the_round_cap_is_reached() {
    let pipeline = load_ok(&GATED.replace("version: 1", "version: 1\nfix_rounds: 2"));
    let mut run = state(&[("ci", FAIL)]);
    run.fix_round = 1;
    assert_eq!(pipeline.plan(&run).decision("fix"), Some(&Decision::Start));
    run.fix_round = 2;
    assert_eq!(
        pipeline.plan(&run).decision("fix"),
        Some(&Decision::Skip(SkipReason::RoundCap))
    );
}

#[test]
fn conditions_see_waivers_as_pass() {
    let pipeline = load_ok(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  review: { uses: claude, needs: [ci] }
gate: [ci]
"#,
    );
    let mut run = state(&[("ci", settled(Verdict::Inconclusive))]);
    run.waived.insert("ci".into());
    assert_eq!(
        pipeline.plan(&run).decision("review"),
        Some(&Decision::Start)
    );
}
