//! The Pipeline editor's gestures: each turns into edits on the draft's
//! text, and a gesture that would make the Pipeline invalid is refused with
//! core's own reason.

mod support;

use serde_json::{Map, json};
use slopwatch_core::{
    EMPTY_PIPELINE, Edit, EditRefusal, GateRole, Outline, Target, Uses, check, try_edits,
};
use support::{TestResolver, load_ok};

const FILE: &str = r#"version: 1
steps:
  ci:     { uses: ci }
  desc:   { uses: jev }
  review: { uses: claude, needs: [ci] }   # once CI is green
  notes:  { uses: lib/notes, needs: [ci], when: [ci] }
  merge:  { uses: merge, needs: [gate] }
  fix:    { uses: fix, needs: [gate] }
gate:
  - ci
  - or: [desc, review]
"#;

fn resolver() -> TestResolver {
    TestResolver::default().with_library("notes", "uses: claude\nwith: { model: haiku }\n")
}

fn outline(text: &str) -> Outline {
    Outline::parse(text).unwrap()
}

/// Applies a gesture's edits the way the daemon does, and fails the test if
/// they're refused.
fn accepted(text: &str, edits: &[Edit]) -> String {
    try_edits(text, edits, &resolver()).unwrap_or_else(|refusal| panic!("refused: {refusal}"))
}

fn refused(text: &str, edits: &[Edit]) -> String {
    match try_edits(text, edits, &resolver()) {
        Ok(after) => panic!("expected a refusal, got:\n{after}"),
        Err(refusal) => refusal.to_string(),
    }
}

fn gate(text: &str) -> Vec<String> {
    let pipeline = slopwatch_core::load(text, &resolver()).unwrap();
    pipeline
        .gate_terms()
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn reads_steps_in_file_order_with_their_gate_roles() {
    let outline = outline(FILE);
    let ids: Vec<&str> = outline.steps().iter().map(|s| s.id.as_str()).collect();

    assert_eq!(ids, ["ci", "desc", "review", "notes", "merge", "fix"]);
    assert_eq!(outline.step("review").unwrap().needs, ["ci"]);
    assert_eq!(outline.gate_role("ci"), GateRole::Required);
    assert_eq!(outline.gate_role("desc"), GateRole::AnyOf);
    assert_eq!(outline.gate_role("notes"), GateRole::Advisory);
}

#[test]
fn dropping_a_library_step_adds_it_to_the_draft() {
    let (id, edits) = outline(FILE).add_step("lib/notes", &["review"]);
    let after = accepted(FILE, &edits);

    assert_eq!(
        id, "notes-2",
        "the id doesn't clash with the Step already there"
    );
    let pipeline = slopwatch_core::load(&after, &resolver()).unwrap();
    let step = pipeline.step("notes-2").unwrap();
    assert_eq!(step.uses, Uses::Library("notes".into()));
    assert_eq!(step.needs, ["review"]);
    assert!(
        !pipeline.gate_reads("notes-2"),
        "new Steps start outside the Gate"
    );
}

#[test]
fn a_new_step_never_takes_a_reserved_id() {
    let (id, _) = outline(EMPTY_PIPELINE).add_step("lib/gate", &[]);
    assert_eq!(id, "gate-2");
}

#[test]
fn the_first_steps_go_into_an_empty_pipeline_even_while_its_gate_is_empty() {
    let (ci, edits) = outline(EMPTY_PIPELINE).add_step("ci", &[]);
    let after = accepted(EMPTY_PIPELINE, &edits);
    let (review, edits) = outline(&after).add_step("claude", &[&ci]);
    let after = accepted(&after, &edits);
    let edits = outline(&after).connect(&ci, Target::Gate);
    let after = accepted(&after, &edits);

    assert_eq!(review, "claude");
    assert_eq!(
        after,
        "version: 1\nsteps:\n  ci: { uses: ci }\n  claude: { uses: claude, needs: [ci] }\ngate: [ci]\n"
    );
    load_ok(&after);
}

#[test]
fn wiring_a_port_to_a_step_adds_a_needs_edge() {
    let edits = outline(FILE).connect("desc", Target::Step("review"));
    let after = accepted(FILE, &edits);

    assert_eq!(
        outline(&after).step("review").unwrap().needs,
        ["ci", "desc"]
    );
    assert!(
        outline(&after)
            .connect("desc", Target::Step("review"))
            .is_empty(),
        "an edge that's there already changes nothing"
    );
}

#[test]
fn wiring_a_port_to_the_gate_adds_a_term_and_to_its_any_of_box_joins_the_group() {
    let after = accepted(FILE, &outline(FILE).connect("notes", Target::Gate));
    assert_eq!(gate(&after), ["ci", "{or: [desc, review]}", "notes"]);

    let after = accepted(FILE, &outline(FILE).connect("notes", Target::GateAnyOf));
    assert_eq!(gate(&after), ["ci", "{or: [desc, review, notes]}"]);

    let after = accepted(FILE, &outline(FILE).connect("ci", Target::GateAnyOf));
    assert_eq!(gate(&after), ["{or: [desc, review, ci]}"]);
}

#[test]
fn wiring_a_cycle_is_refused_with_the_reason() {
    let edits = outline(FILE).connect("review", Target::Step("ci"));

    assert_eq!(
        refused(FILE, &edits),
        "the Pipeline has a cycle: ci -> review -> ci"
    );
}

#[test]
fn wiring_a_write_step_into_the_gate_is_refused_with_the_reason() {
    // A write Step that doesn't need the Gate, so no cycle comes first.
    let text = FILE.replace("gate:\n", "  autofix: { uses: fix }\ngate:\n");
    let edits = outline(&text).connect("autofix", Target::Gate);

    assert_eq!(
        refused(&text, &edits),
        "the Gate references `autofix`, which declares `workspace: write`"
    );
    assert_eq!(
        refused(FILE, &outline(FILE).connect("fix", Target::Gate)),
        "the Pipeline has a cycle: fix -> gate -> fix"
    );
}

#[test]
fn the_gate_reading_a_step_that_runs_after_it_is_refused() {
    let edits = outline(FILE).connect("merge", Target::Gate);

    assert_eq!(
        refused(FILE, &edits),
        "the Pipeline has a cycle: gate -> merge -> gate"
    );
}

#[test]
fn a_cycle_is_refused_even_while_the_gate_is_empty() {
    let text = "version: 1\nsteps:\n  a: { uses: ci }\n  b: { uses: ci, needs: [a] }\ngate: []\n";
    let edits = outline(text).connect("b", Target::Step("a"));

    assert_eq!(
        refused(text, &edits),
        "the Pipeline has a cycle: a -> b -> a"
    );
}

#[test]
fn errors_the_draft_already_has_dont_block_other_gestures() {
    let text = "version: 1\nsteps:\n  a: { uses: lib/gone }\n  b: { uses: ci }\ngate: [b]\n";
    let before = check(text, &resolver());
    assert_eq!(
        before.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["Step `a` uses Library Step `gone`, which doesn't exist"]
    );

    accepted(text, &outline(text).connect("a", Target::Step("b")));
    // The Step that doesn't resolve still takes part in the graph rules.
    let text = accepted(text, &outline(text).connect("a", Target::Step("b")));
    assert_eq!(
        refused(&text, &outline(&text).connect("b", Target::Step("a"))),
        "the Pipeline has a cycle: a -> b -> a"
    );
}

#[test]
fn switching_a_steps_gate_role_keeps_its_place_in_the_gate() {
    let edits = outline(FILE).set_gate_role("ci", GateRole::RequiredOrSkipped);
    let after = accepted(FILE, &edits);
    assert_eq!(
        after
            .lines()
            .filter(|l| l.contains("ci:") || l.starts_with("  -"))
            .collect::<Vec<_>>(),
        [
            "  ci:     { uses: ci }",
            "  - { ci: [pass, skipped] }",
            "  - or: [desc, review]"
        ]
    );
    assert_eq!(outline(&after).gate_role("ci"), GateRole::RequiredOrSkipped);

    let back = accepted(
        &after,
        &outline(&after).set_gate_role("ci", GateRole::Required),
    );
    assert_eq!(back, FILE);
}

#[test]
fn making_a_step_advisory_takes_it_out_of_the_gate_and_drops_an_emptied_group() {
    let after = accepted(
        FILE,
        &outline(FILE).set_gate_role("desc", GateRole::Advisory),
    );
    assert_eq!(gate(&after), ["ci", "{or: [review]}"]);

    let after = accepted(
        &after,
        &outline(&after).set_gate_role("review", GateRole::Advisory),
    );
    assert_eq!(gate(&after), ["ci"]);
    assert_eq!(outline(&after).gate_role("review"), GateRole::Advisory);
}

#[test]
fn removing_the_last_gate_term_is_allowed_and_leaves_the_gate_empty() {
    let text = "version: 1\nsteps:\n  ci: { uses: ci }\ngate:\n  - ci\n";
    let after = accepted(text, &outline(text).set_gate_role("ci", GateRole::Advisory));

    assert_eq!(after, "version: 1\nsteps:\n  ci: { uses: ci }\ngate: []\n");
}

#[test]
fn removing_a_step_unwires_it_everywhere() {
    let edits = outline(FILE).remove_step("ci");
    let text = FILE.replace(
        "  notes:  { uses: lib/notes, needs: [ci], when: [ci] }\n",
        "",
    );
    let edits_without_notes = outline(&text).remove_step("ci");
    let after = accepted(&text, &edits_without_notes);

    assert!(outline(&after).step("ci").is_none());
    assert!(outline(&after).step("review").unwrap().needs.is_empty());
    assert_eq!(gate(&after), ["{or: [desc, review]}"]);
    // With `notes` still reading it in its Condition, removing it is refused.
    assert_eq!(
        refused(FILE, &edits),
        "the Condition of Step `notes` references `ci`, which isn't a Step"
    );
}

#[test]
fn inspector_edits_change_with_needs_the_condition_and_uses() {
    let with = Map::from_iter([("model".to_owned(), json!("sonnet"))]);
    let mut edits = outline(FILE).set_with("review", &with);
    edits.extend(outline(FILE).toggle_need("review", "desc"));
    edits.extend(outline(FILE).set_condition("review", Some(&json!({ "or": ["ci", "desc"] }))));
    edits.extend(outline(FILE).set_uses("desc", "lib/notes"));
    let after = accepted(FILE, &edits);

    let review = outline(&after).step("review").unwrap().clone();
    assert_eq!(review.with, with);
    assert_eq!(review.needs, ["ci", "desc"]);
    assert_eq!(review.when, Some(json!({ "or": ["ci", "desc"] })));
    assert_eq!(outline(&after).step("desc").unwrap().uses, "lib/notes");

    // Emptying `with` and the Condition takes the keys out again.
    let mut edits = outline(&after).set_with("review", &Map::new());
    edits.extend(outline(&after).set_condition("review", None));
    edits.extend(outline(&after).toggle_need("review", "desc"));
    let back = accepted(&after, &edits);
    assert_eq!(
        back.lines().nth(4),
        Some("  review: { uses: claude, needs: [ci] }   # once CI is green")
    );
}

#[test]
fn edits_travel_as_json() {
    let edits = vec![
        Edit::AddNeed {
            step: "review".into(),
            need: "ci".into(),
        },
        Edit::SetFixRounds(Some(2)),
    ];
    let wire = serde_json::to_value(&edits).unwrap();

    assert_eq!(
        wire,
        json!([
            { "add_need": { "step": "review", "need": "ci" } },
            { "set_fix_rounds": 2 },
        ])
    );
    assert_eq!(serde_json::from_value::<Vec<Edit>>(wire).unwrap(), edits);
}

#[test]
fn edits_that_dont_fit_the_file_are_refused() {
    let edits = [Edit::RemoveStep { id: "nope".into() }];

    assert!(matches!(
        try_edits(FILE, &edits, &resolver()),
        Err(EditRefusal::Edit(_))
    ));
    assert_eq!(refused(FILE, &edits), "Step `nope` isn't in the Pipeline");
}
