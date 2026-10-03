//! Replaying a draft's edits onto the Pipeline file as the branch has it
//! now (ADR 0007): the branch's own changes and comments survive, and a
//! node changed on both sides stops the replay with both versions.

use serde_json::{Map, Value, json};
use slopwatch_core::{
    Conflict, EMPTY_PIPELINE, Edit, Node, ReplayError, Replayed, apply_edits, replay,
};

/// The file when the draft started.
const BASE: &str = "\
version: 1
steps:
  # Every PR.
  ci: { uses: ci }
  review:
    uses: claude
    needs: [ci]
gate: [ci, review]
";

fn step(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("not an object"),
    }
}

/// The draft: a `lint` Step the Gate requires, and `review` on opus.
fn edits() -> Vec<Edit> {
    vec![
        Edit::AddStep {
            id: "lint".into(),
            step: step(json!({ "uses": "ci" })),
        },
        Edit::AddGateTerm {
            term: "lint".into(),
        },
        Edit::SetWith {
            step: "review".into(),
            key: "model".into(),
            value: "opus".into(),
        },
    ]
}

fn replayed(base: &str, edits: &[Edit], now: &str) -> Replayed {
    replay(base, edits, now).unwrap_or_else(|error| panic!("{error}"))
}

fn conflicts(base: &str, edits: &[Edit], now: &str) -> Vec<Conflict> {
    match replay(base, edits, now) {
        Err(ReplayError::Conflicts(conflicts)) => conflicts,
        other => panic!("expected conflicts, got {other:?}"),
    }
}

#[test]
fn on_an_unchanged_branch_the_replay_is_the_draft() {
    let replayed = replayed(BASE, &edits(), BASE);

    assert_eq!(replayed.text, apply_edits(BASE, &edits()).unwrap());
    assert_eq!(replayed.edits, edits());
}

#[test]
fn the_branchs_own_changes_and_comments_stay_under_the_draft() {
    // Since the draft started: a comment, a new Step, `ci`'s timeout.
    let now = "\
# Owned by the platform team.
version: 1
steps:
  # Every PR.
  ci: { uses: ci, timeout: 20m }
  review:
    uses: claude
    needs: [ci]
  docs: { uses: jev }   # docs only
gate: [ci, review]
";

    let replayed = replayed(BASE, &edits(), now);

    assert_eq!(
        replayed.text,
        "\
# Owned by the platform team.
version: 1
steps:
  # Every PR.
  ci: { uses: ci, timeout: 20m }
  review:
    uses: claude
    needs: [ci]
    with: { model: opus }
  docs: { uses: jev }   # docs only
  lint: { uses: ci }
gate: [ci, review, lint]
"
    );
    assert_eq!(replayed.edits, edits());
}

#[test]
fn a_step_edited_on_both_sides_stops_with_both_versions() {
    let now = BASE.replace("    needs: [ci]\n", "    needs: [ci]\n    timeout: 1h\n");

    let found = conflicts(BASE, &edits(), &now);

    assert_eq!(
        found,
        [Conflict {
            node: Node::Step("review".into()),
            draft: Some("review:\n  uses: claude\n  needs: [ci]\n  with: { model: opus }".into()),
            branch: Some("review:\n  uses: claude\n  needs: [ci]\n  timeout: 1h".into()),
        }]
    );
    assert_eq!(
        replay(BASE, &edits(), &now).unwrap_err().to_string(),
        "Step `review` changed on the branch since the draft started, and the draft changes it too"
    );
}

#[test]
fn the_gate_edited_on_both_sides_is_a_conflict() {
    let now = BASE.replace("gate: [ci, review]", "gate: [ci]");

    let found = conflicts(BASE, &edits(), &now);

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].node, Node::Gate);
    assert_eq!(found[0].draft.as_deref(), Some("gate: [ci, review, lint]"));
    assert_eq!(found[0].branch.as_deref(), Some("gate: [ci]"));
}

#[test]
fn a_step_the_branch_removed_conflicts_with_editing_it() {
    let now = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci, review]\n";
    let edit = [Edit::SetWith {
        step: "review".into(),
        key: "model".into(),
        value: "opus".into(),
    }];

    let found = conflicts(BASE, &edit, now);

    assert_eq!(found[0].node, Node::Step("review".into()));
    assert_eq!(found[0].branch, None);
}

#[test]
fn a_step_both_sides_added_under_one_id_conflicts_unless_its_the_same() {
    let other = BASE.replace("gate:", "  lint: { uses: jev }\ngate:");
    let add = [Edit::AddStep {
        id: "lint".into(),
        step: step(json!({ "uses": "ci" })),
    }];
    assert_eq!(
        conflicts(BASE, &add, &other)[0].branch.as_deref(),
        Some("lint: { uses: jev }")
    );

    let same = BASE.replace("gate:", "  lint: { uses: ci }\ngate:");
    let replayed = replayed(BASE, &add, &same);
    assert_eq!(replayed.text, same);
    assert!(replayed.edits.is_empty());
}

#[test]
fn a_draft_the_branch_already_has_replays_to_the_branch() {
    // The draft's PR merged, with a comment the developer added on GitHub.
    let now = apply_edits(BASE, &edits())
        .unwrap()
        .replace("gate:", "# Lint too.\ngate:");

    let replayed = replayed(BASE, &edits(), &now);

    assert_eq!(replayed.text, now);
    assert!(replayed.edits.is_empty());
}

#[test]
fn a_comment_on_a_node_isnt_a_change_to_it() {
    let now = BASE.replace("    needs: [ci]\n", "    needs: [ci]   # after CI\n");

    let replayed = replayed(BASE, &edits(), &now);

    assert!(
        replayed
            .text
            .contains("needs: [ci]   # after CI\n    with: { model: opus }\n"),
        "{}",
        replayed.text
    );
}

#[test]
fn a_draft_started_without_a_pipeline_replays_onto_one_that_landed_since() {
    let add = [
        Edit::AddStep {
            id: "lint".into(),
            step: step(json!({ "uses": "ci" })),
        },
        Edit::AddGateTerm {
            term: "lint".into(),
        },
    ];
    let now = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n";

    // Both sides wrote a Gate.
    assert_eq!(conflicts(EMPTY_PIPELINE, &add, now)[0].node, Node::Gate);

    let replayed = replayed(EMPTY_PIPELINE, &add[..1], now);
    assert_eq!(
        replayed.text,
        "version: 1\nsteps:\n  ci: { uses: ci }\n  lint: { uses: ci }\ngate: [ci]\n"
    );
}

#[test]
fn nodes_sort_steps_by_id_before_the_gate() {
    let touched: Vec<String> = Node::touched(&edits())
        .iter()
        .map(ToString::to_string)
        .collect();

    assert_eq!(touched, ["Step `lint`", "Step `review`", "the Gate"]);
    assert_eq!(
        serde_json::to_value(Node::Step("ci".into())).unwrap(),
        json!({ "step": "ci" })
    );
    assert_eq!(serde_json::to_value(Node::Gate).unwrap(), json!("gate"));
}
