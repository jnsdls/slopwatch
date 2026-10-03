//! Keeps the `yaml-edit` editor and the `serde-saphyr` loader in agreement
//! (ADR 0007). Each case applies editor edits to a hand-written Pipeline,
//! then checks two things: the loader reads the result as the intended
//! Pipeline, and every line the edit didn't touch comes back byte for byte.

mod support;

use serde_json::{Value, json};
use slopwatch_core::{Edit, EditError, Pipeline, apply_edits};
use support::{TestResolver, load_ok};

/// A hand-written Pipeline: comments everywhere, block and flow styles mixed,
/// a nested `or:` in a Condition, blank lines between groups.
const FILE: &str = r#"# The Pipeline for this repo.
version: 1

steps:
  # Checks every PR gets.
  ci:     { uses: ci }
  desc:   { uses: jev, with: { questions: [desc-matches-diff] } }  # a preset
  review:
    uses: claude
    needs: [ci]   # only once CI is green
    with:
      model: opus
      focus: [security, tests]
  docs:   { uses: jev, when: { files: "docs/**" } }
  human:
    uses: human
    needs:
      - review
      - desc
    when:
      or:
        - review
        - and: [desc, { not: [review] }]
    with: { prompt: "Ship it?" }

  # After the Gate.
  merge:  { uses: merge, needs: [gate], when: gate }
  fix:    { uses: fix, needs: [gate] }   # agent defaults to claude

gate:
  - ci
  - desc
  # Either reviewer will do.
  - or: [review, human]
  - docs: [pass, skipped]
"#;

/// The same kinds of nodes written in flow style: a flow Gate with a nested
/// `or:`, flow `needs`, and a comment after the last Step.
const FLOW_FILE: &str = r#"version: 1
fix_rounds: 2   # keep the loop short
gate: [ci, { or: [review, { and: [lint, ci] }] }]  # the whole Gate
steps:
  ci: { uses: ci }
  lint: { uses: ci, with: { job: lint } }
  review: { uses: claude, needs: [ci, lint] }

# Nothing after the Gate yet.
"#;

#[test]
fn the_files_load_unedited() {
    assert_eq!(apply_edits(FILE, &[]).unwrap(), FILE);
    assert_eq!(apply_edits(FLOW_FILE, &[]).unwrap(), FLOW_FILE);
    load_ok(FILE);
    load_ok(FLOW_FILE);
}

// Inserting and removing Steps.

#[test]
fn adds_a_step_after_the_last_one() {
    let edits = [Edit::AddStep {
        id: "lint".into(),
        step: object(json!({ "needs": ["ci"], "uses": "ci", "with": { "job": "lint" } })),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[],
        &["  lint: { uses: ci, needs: [ci], with: { job: lint } }"],
    );
    let lint = load_ok(&after);
    let lint = lint.step("lint").unwrap();
    assert_eq!(lint.needs, ["ci"]);
    assert_eq!(lint.config["job"], "lint");
}

#[test]
fn adds_a_step_to_a_file_that_ends_in_a_comment() {
    let edits = [Edit::AddStep {
        id: "merge".into(),
        step: object(json!({ "uses": "merge", "needs": ["gate"], "when": "gate" })),
    }];
    let after = edit(FLOW_FILE, &edits);
    assert_diff(
        FLOW_FILE,
        &after,
        &[],
        &["  merge: { uses: merge, needs: [gate], when: gate }"],
    );
    assert_loads_as(
        &after,
        r#"
version: 1
fix_rounds: 2
gate: [ci, { or: [review, { and: [lint, ci] }] }]
steps:
  ci: { uses: ci }
  lint: { uses: ci, with: { job: lint } }
  review: { uses: claude, needs: [ci, lint] }
  merge: { uses: merge, needs: [gate], when: gate }
"#,
    );
}

#[test]
fn adds_a_step_and_wires_it_into_the_gate() {
    let edits = [
        Edit::AddStep {
            id: "codex".into(),
            step: object(json!({ "uses": "codex", "needs": ["ci"] })),
        },
        Edit::AddGateTerm {
            term: json!("codex"),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[],
        &["  codex: { uses: codex, needs: [ci] }", "  - codex"],
    );
    let pipeline = load_ok(&after);
    assert_eq!(gate(&pipeline).last().unwrap(), "codex");
}

#[test]
fn removes_a_flow_step_and_keeps_the_comment_above_it() {
    let edits = [
        Edit::RemoveStep { id: "ci".into() },
        Edit::RemoveNeed {
            step: "review".into(),
            need: "ci".into(),
        },
        Edit::RemoveGateTerm { index: 0 },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "  ci:     { uses: ci }",
            "    needs: [ci]   # only once CI is green",
            "  - ci",
        ],
        &[],
    );
    let pipeline = load_ok(&after);
    assert!(pipeline.step("ci").is_none());
    assert!(pipeline.step("review").unwrap().needs.is_empty());
}

#[test]
fn removes_a_block_step_and_its_gate_term() {
    let edits = [
        Edit::RemoveStep { id: "human".into() },
        Edit::SetGateTerm {
            index: 2,
            term: json!("review"),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "  human:",
            "    uses: human",
            "    needs:",
            "      - review",
            "      - desc",
            "    when:",
            "      or:",
            "        - review",
            "        - and: [desc, { not: [review] }]",
            "    with: { prompt: \"Ship it?\" }",
            "  - or: [review, human]",
        ],
        &["  - review"],
    );
    assert_eq!(
        gate(&load_ok(&after)),
        ["ci", "desc", "review", "{docs: [pass, skipped]}"]
    );
}

#[test]
fn removes_the_last_step() {
    let after = edit(FILE, &[Edit::RemoveStep { id: "fix".into() }]);
    assert_diff(
        FILE,
        &after,
        &["  fix:    { uses: fix, needs: [gate] }   # agent defaults to claude"],
        &[],
    );
    assert!(load_ok(&after).step("fix").is_none());
}

// Setting and removing keys.

#[test]
fn sets_a_key_on_a_block_step() {
    let edits = [Edit::SetKey {
        step: "review".into(),
        key: "timeout".into(),
        value: json!("30m"),
    }];
    let after = edit(FILE, &edits);
    assert_diff(FILE, &after, &[], &["    timeout: 30m"]);
    let pipeline = load_ok(&after);
    assert_eq!(
        pipeline.step("review").unwrap().timeout,
        Some(std::time::Duration::from_secs(30 * 60))
    );
}

#[test]
fn sets_a_key_on_a_flow_step() {
    let edits = [Edit::SetKey {
        step: "desc".into(),
        key: "uses".into(),
        value: json!("lib/desc-matches-diff"),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  desc:   { uses: jev, with: { questions: [desc-matches-diff] } }  # a preset"],
        &[
            "  desc:   { uses: lib/desc-matches-diff, with: { questions: [desc-matches-diff] } }  # a preset",
        ],
    );
    let resolver = TestResolver::default().with_library("desc-matches-diff", "uses: jev\n");
    let pipeline = slopwatch_core::load(&after, &resolver).unwrap();
    assert_eq!(pipeline.step("desc").unwrap().plugin, "jev");
}

#[test]
fn replaces_a_keys_value() {
    let edits = [Edit::SetKey {
        step: "docs".into(),
        key: "when".into(),
        value: json!({ "files": ["docs/**", "**/*.md"] }),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  docs:   { uses: jev, when: { files: \"docs/**\" } }"],
        &["  docs:   { uses: jev, when: { files: [docs/**, \"**/*.md\"] } }"],
    );
    let pipeline = load_ok(&after);
    assert_eq!(
        pipeline.step("docs").unwrap().condition().to_string(),
        "{files: [docs/**, **/*.md]}"
    );
}

#[test]
fn removes_a_key() {
    let edits = [
        Edit::RemoveKey {
            step: "docs".into(),
            key: "when".into(),
        },
        Edit::RemoveKey {
            step: "human".into(),
            key: "when".into(),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "  docs:   { uses: jev, when: { files: \"docs/**\" } }",
            "    when:",
            "      or:",
            "        - review",
            "        - and: [desc, { not: [review] }]",
        ],
        &["  docs:   { uses: jev }"],
    );
    let pipeline = load_ok(&after);
    assert_eq!(pipeline.step("docs").unwrap().condition().to_string(), "[]");
    assert_eq!(
        pipeline.step("human").unwrap().condition().to_string(),
        "[review, desc]"
    );
}

#[test]
fn sets_with_overrides_in_block_and_flow_mappings() {
    let edits = [
        Edit::SetWith {
            step: "review".into(),
            key: "model".into(),
            value: json!("sonnet"),
        },
        Edit::SetWith {
            step: "human".into(),
            key: "prompt".into(),
            value: json!("Merge it?\nSay why."),
        },
        Edit::SetWith {
            step: "ci".into(),
            key: "job".into(),
            value: json!("build"),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "  ci:     { uses: ci }",
            "      model: opus",
            "    with: { prompt: \"Ship it?\" }",
        ],
        &[
            "  ci:     { uses: ci, with: { job: build } }",
            "      model: sonnet",
            "    with: { prompt: \"Merge it?\\nSay why.\" }",
        ],
    );
    let pipeline = load_ok(&after);
    assert_eq!(pipeline.step("review").unwrap().config["model"], "sonnet");
    assert_eq!(
        pipeline.step("human").unwrap().config["prompt"],
        "Merge it?\nSay why."
    );
    assert_eq!(pipeline.step("ci").unwrap().config["job"], "build");
}

#[test]
fn removing_the_last_with_override_removes_with() {
    let edits = [
        Edit::RemoveWith {
            step: "review".into(),
            key: "focus".into(),
        },
        Edit::RemoveWith {
            step: "human".into(),
            key: "prompt".into(),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "      focus: [security, tests]",
            "    with: { prompt: \"Ship it?\" }",
        ],
        &[],
    );
    let pipeline = load_ok(&after);
    assert_eq!(
        Value::Object(pipeline.step("review").unwrap().config.clone()),
        json!({ "model": "opus" })
    );
    assert!(pipeline.step("human").unwrap().config.is_empty());
}

#[test]
fn quotes_strings_that_would_not_read_back_as_strings() {
    let values = [
        json!("true"),
        json!("null"),
        json!("10"),
        json!(""),
        json!("a: b"),
        json!("x # y"),
        json!("[a]"),
        json!("*alias"),
        json!(" padded "),
        json!("Ünïcode"),
        json!("two\nlines"),
    ];
    for value in values {
        let edits = [Edit::SetWith {
            step: "ci".into(),
            key: "v".into(),
            value: value.clone(),
        }];
        let after = edit(FILE, &edits);
        let (removed, added) = line_diff(FILE, &after);
        assert_eq!(removed, ["  ci:     { uses: ci }"], "{after}");
        assert!(
            added.len() == 1 && added[0].starts_with("  ci:     { uses: ci, with: { v: \""),
            "{after}"
        );
        assert_eq!(load_ok(&after).step("ci").unwrap().config["v"], value);
    }
}

// Rewiring `needs`.

#[test]
fn rewires_needs_in_flow_and_block_sequences() {
    let edits = [
        Edit::AddNeed {
            step: "review".into(),
            need: "desc".into(),
        },
        Edit::AddNeed {
            step: "human".into(),
            need: "docs".into(),
        },
        Edit::RemoveNeed {
            step: "human".into(),
            need: "desc".into(),
        },
        Edit::AddNeed {
            step: "docs".into(),
            need: "ci".into(),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &[
            "    needs: [ci]   # only once CI is green",
            "  docs:   { uses: jev, when: { files: \"docs/**\" } }",
            "      - desc",
        ],
        &[
            "    needs: [ci, desc]   # only once CI is green",
            "  docs:   { uses: jev, when: { files: \"docs/**\" }, needs: [ci] }",
            "      - docs",
        ],
    );
    let pipeline = load_ok(&after);
    assert_eq!(pipeline.step("review").unwrap().needs, ["ci", "desc"]);
    assert_eq!(pipeline.step("human").unwrap().needs, ["review", "docs"]);
    assert_eq!(pipeline.step("docs").unwrap().needs, ["ci"]);
}

#[test]
fn removing_the_last_need_removes_needs() {
    let edits = [Edit::RemoveNeed {
        step: "review".into(),
        need: "ci".into(),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["    needs: [ci]   # only once CI is green"],
        &[],
    );
    assert!(load_ok(&after).step("review").unwrap().needs.is_empty());
}

#[test]
fn rewires_flow_needs_at_the_end_of_a_flow_mapping() {
    let edits = [
        Edit::RemoveNeed {
            step: "review".into(),
            need: "lint".into(),
        },
        Edit::AddNeed {
            step: "review".into(),
            need: "gate".into(),
        },
        Edit::SetKey {
            step: "review".into(),
            key: "when".into(),
            value: json!("always"),
        },
        Edit::SetGateTerm {
            index: 1,
            term: json!("lint"),
        },
    ];
    let after = edit(FLOW_FILE, &edits);
    assert_loads_as(
        &after,
        r#"
version: 1
fix_rounds: 2
gate: [ci, lint]
steps:
  ci: { uses: ci }
  lint: { uses: ci, with: { job: lint } }
  review: { uses: claude, needs: [ci, gate], when: always }
"#,
    );
    assert_diff(
        FLOW_FILE,
        &after,
        &[
            "gate: [ci, { or: [review, { and: [lint, ci] }] }]  # the whole Gate",
            "  review: { uses: claude, needs: [ci, lint] }",
        ],
        &[
            "gate: [ci, lint]  # the whole Gate",
            "  review: { uses: claude, needs: [ci, gate], when: always }",
        ],
    );
}

// Nested `or:`.

#[test]
fn writes_a_nested_or_as_a_gate_term() {
    let edits = [Edit::SetGateTerm {
        index: 2,
        term: json!({ "or": ["review", { "and": ["human", { "not": ["docs"] }] }] }),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  - or: [review, human]"],
        &["  - { or: [review, { and: [human, { not: [docs] }] }] }"],
    );
    assert_eq!(
        gate(&load_ok(&after))[2],
        "{or: [review, [human, {not: [docs]}]]}"
    );
}

#[test]
fn writes_a_nested_or_as_a_condition() {
    let edits = [Edit::SetKey {
        step: "merge".into(),
        key: "when".into(),
        value: json!({ "or": ["gate", { "and": [{ "labels": "ship-it" }, { "draft": false }] }] }),
    }];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  merge:  { uses: merge, needs: [gate], when: gate }"],
        &[
            "  merge:  { uses: merge, needs: [gate], when: { or: [gate, { and: [{ labels: ship-it }, { draft: false }] }] } }",
        ],
    );
    assert_eq!(
        load_ok(&after)
            .step("merge")
            .unwrap()
            .condition()
            .to_string(),
        "{or: [gate, [{labels: [ship-it]}, {draft: false}]]}"
    );
}

#[test]
fn edits_inside_a_nested_condition_leave_its_siblings_alone() {
    // Rewiring `human` around the nested `or:` doesn't disturb it.
    let edits = [
        Edit::AddNeed {
            step: "human".into(),
            need: "docs".into(),
        },
        Edit::SetWith {
            step: "human".into(),
            key: "prompt".into(),
            value: json!("Ship?"),
        },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["    with: { prompt: \"Ship it?\" }"],
        &["      - docs", "    with: { prompt: \"Ship?\" }"],
    );
    assert_eq!(
        load_ok(&after)
            .step("human")
            .unwrap()
            .condition()
            .to_string(),
        "{or: [review, [desc, {not: [review]}]]}"
    );
}

// Flow sequences.

#[test]
fn appends_and_removes_terms_of_a_flow_gate() {
    let edits = [
        Edit::AddGateTerm {
            term: json!({ "review": ["pass", "skipped"] }),
        },
        Edit::RemoveGateTerm { index: 0 },
    ];
    let after = edit(FLOW_FILE, &edits);
    assert_diff(
        FLOW_FILE,
        &after,
        &["gate: [ci, { or: [review, { and: [lint, ci] }] }]  # the whole Gate"],
        &[
            "gate: [{ or: [review, { and: [lint, ci] }] }, { review: [pass, skipped] }]  # the whole Gate",
        ],
    );
    assert_eq!(
        gate(&load_ok(&after)),
        ["{or: [review, [lint, ci]]}", "{review: [pass, skipped]}"]
    );
}

#[test]
fn appends_to_a_block_gate_at_the_end_of_the_file() {
    let term = json!({ "human": ["pass", "skipped"] });
    let after = edit(FILE, &[Edit::AddGateTerm { term }]);
    assert_diff(FILE, &after, &[], &["  - { human: [pass, skipped] }"]);
    assert!(after.ends_with("  - docs: [pass, skipped]\n  - { human: [pass, skipped] }\n"));
    assert_eq!(gate(&load_ok(&after))[4], "{human: [pass, skipped]}");
}

#[test]
fn removes_a_middle_flow_item_and_fills_empty_flow_collections() {
    let file = "version: 1\nsteps:\n  ci: { uses: ci, with: {} }\n  lint: { uses: ci, needs: [] }\n  a: { uses: jev }\ngate: [ci, a, lint]  # all three\n";
    let edits = [
        Edit::RemoveGateTerm { index: 1 },
        Edit::SetWith {
            step: "ci".into(),
            key: "job".into(),
            value: json!("build"),
        },
        Edit::AddNeed {
            step: "lint".into(),
            need: "ci".into(),
        },
    ];
    let after = edit(file, &edits);
    assert_diff(
        file,
        &after,
        &[
            "  ci: { uses: ci, with: {} }",
            "  lint: { uses: ci, needs: [] }",
            "gate: [ci, a, lint]  # all three",
        ],
        &[
            "  ci: { uses: ci, with: { job: build } }",
            "  lint: { uses: ci, needs: [ci] }",
            "gate: [ci, lint]  # all three",
        ],
    );
    assert_loads_as(
        &after,
        "version: 1\nsteps:\n  ci: { uses: ci, with: { job: build } }\n  lint: { uses: ci, needs: [ci] }\n  a: { uses: jev }\ngate: [ci, lint]\n",
    );
}

#[test]
fn sets_a_key_on_a_block_step_followed_by_a_blank_line_and_a_comment() {
    let edits = [Edit::SetKey {
        step: "human".into(),
        key: "timeout".into(),
        value: json!("2h"),
    }];
    let after = edit(FILE, &edits);
    assert_diff(FILE, &after, &[], &["    timeout: 2h"]);
    assert!(
        after.contains("    timeout: 2h\n\n  # After the Gate.\n"),
        "{after}"
    );
    assert_eq!(
        load_ok(&after).step("human").unwrap().timeout,
        Some(std::time::Duration::from_secs(2 * 3600))
    );
}

#[test]
fn replaces_a_block_value_followed_by_a_blank_line_and_a_comment() {
    let file = "version: 1\nsteps:\n  ci:\n    uses: ci\n    with:\n      job: lint\n\n  # Later.\n  b: { uses: jev }\ngate: [ci]\n";
    let edits = [
        Edit::SetKey {
            step: "ci".into(),
            key: "with".into(),
            value: json!({ "job": "build" }),
        },
        Edit::SetWith {
            step: "ci".into(),
            key: "os".into(),
            value: json!("macos"),
        },
    ];
    let after = edit(file, &edits);
    assert_diff(
        file,
        &after,
        &["    with:", "      job: lint"],
        &["    with: { job: build, os: macos }"],
    );
    assert_loads_as(
        &after,
        "version: 1\nsteps:\n  ci: { uses: ci, with: { job: build, os: macos } }\n  b: { uses: jev }\ngate: [ci]\n",
    );
}

#[test]
fn fills_an_empty_flow_gate_that_has_a_comment() {
    let file = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: []  # none yet\n";
    let after = edit(file, &[Edit::AddGateTerm { term: json!("ci") }]);
    assert_diff(
        file,
        &after,
        &["gate: []  # none yet"],
        &["gate: [ci]  # none yet"],
    );
    assert_eq!(gate(&load_ok(&after)), ["ci"]);
}

// Empty collections, as a new Pipeline starts.

/// What a repo's draft starts from when it has no Pipeline file yet.
const EMPTY: &str = "version: 1\nsteps: {}\ngate: []\n";

#[test]
fn the_first_step_turns_empty_steps_into_a_block_mapping() {
    let edits = [
        Edit::AddStep {
            id: "ci".into(),
            step: object(json!({ "uses": "ci" })),
        },
        Edit::AddStep {
            id: "review".into(),
            step: object(json!({ "uses": "claude", "needs": ["ci"] })),
        },
        Edit::AddGateTerm { term: json!("ci") },
    ];
    let after = edit(EMPTY, &edits);
    assert_eq!(
        after,
        "version: 1\nsteps:\n  ci: { uses: ci }\n  review: { uses: claude, needs: [ci] }\ngate: [ci]\n"
    );
    load_ok(&after);
}

#[test]
fn removing_the_only_step_and_gate_term_leaves_them_empty() {
    let file = "version: 1\nsteps:\n  ci: { uses: ci }  # the one Step\n\n# Ship when CI passes.\ngate:\n  - ci\n";
    let edits = [
        Edit::RemoveGateTerm { index: 0 },
        Edit::RemoveStep { id: "ci".into() },
    ];
    let after = edit(file, &edits);
    assert_eq!(
        after,
        "version: 1\nsteps: {}\n\n# Ship when CI passes.\ngate: []\n"
    );
    // And the editor can fill them again.
    let edits = [
        Edit::AddStep {
            id: "lint".into(),
            step: object(json!({ "uses": "ci" })),
        },
        Edit::AddGateTerm {
            term: json!("lint"),
        },
    ];
    let refilled = edit(&after, &edits);
    assert_eq!(
        refilled,
        "version: 1\nsteps:\n  lint: { uses: ci }\n\n# Ship when CI passes.\ngate: [lint]\n"
    );
    load_ok(&refilled);
}

#[test]
fn removing_the_only_flow_gate_term_leaves_an_empty_flow_gate() {
    let file = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]  # just CI\n";
    let after = edit(file, &[Edit::RemoveGateTerm { index: 0 }]);
    assert_eq!(
        after,
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: []  # just CI\n"
    );
}

// Comments.

#[test]
fn removing_a_gate_term_keeps_the_comments_around_it() {
    let edits = [
        Edit::RemoveGateTerm { index: 3 },
        Edit::RemoveGateTerm { index: 1 },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  - desc", "  - docs: [pass, skipped]"],
        &[],
    );
    assert_eq!(gate(&load_ok(&after)), ["ci", "{or: [review, human]}"]);
}

#[test]
fn removing_the_gate_term_under_a_comment_keeps_the_line_above() {
    let edits = [
        Edit::RemoveGateTerm { index: 2 },
        Edit::RemoveGateTerm { index: 2 },
    ];
    let after = edit(FILE, &edits);
    assert_diff(
        FILE,
        &after,
        &["  - or: [review, human]", "  - docs: [pass, skipped]"],
        &[],
    );
    assert_eq!(gate(&load_ok(&after)), ["ci", "desc"]);
}

#[test]
fn sets_and_removes_fix_rounds_keeping_its_comment_neighbours() {
    let set = edit(FILE, &[Edit::SetFixRounds(Some(5))]);
    assert_diff(FILE, &set, &[], &["fix_rounds: 5"]);
    assert_eq!(load_ok(&set).fix_rounds(), 5);

    let removed = edit(FLOW_FILE, &[Edit::SetFixRounds(None)]);
    assert_diff(
        FLOW_FILE,
        &removed,
        &["fix_rounds: 2   # keep the loop short"],
        &[],
    );
    assert_eq!(load_ok(&removed).fix_rounds(), 3);
}

#[test]
fn a_long_draft_replays_onto_the_file() {
    let edits = [
        Edit::AddStep {
            id: "codex".into(),
            step: object(json!({ "uses": "codex", "needs": ["ci"], "with": { "model": "gpt-5" } })),
        },
        Edit::SetGateTerm {
            index: 2,
            term: json!({ "or": ["review", "codex", "human"] }),
        },
        Edit::RemoveStep { id: "docs".into() },
        Edit::RemoveGateTerm { index: 3 },
        Edit::SetKey {
            step: "human".into(),
            key: "when".into(),
            value: json!({ "or": ["review", "codex"] }),
        },
        Edit::AddNeed {
            step: "human".into(),
            need: "codex".into(),
        },
        Edit::SetWith {
            step: "fix".into(),
            key: "agent".into(),
            value: json!("codex"),
        },
        Edit::SetFixRounds(Some(4)),
    ];
    let after = edit(FILE, &edits);
    assert_loads_as(
        &after,
        r#"
version: 1
fix_rounds: 4
steps:
  ci:     { uses: ci }
  desc:   { uses: jev, with: { questions: [desc-matches-diff] } }
  review: { uses: claude, needs: [ci], with: { model: opus, focus: [security, tests] } }
  human:  { uses: human, needs: [review, desc, codex], when: { or: [review, codex] }, with: { prompt: "Ship it?" } }
  merge:  { uses: merge, needs: [gate], when: gate }
  fix:    { uses: fix, needs: [gate], with: { agent: codex } }
  codex:  { uses: codex, needs: [ci], with: { model: gpt-5 } }
gate:
  - ci
  - desc
  - or: [review, codex, human]
"#,
    );
    assert_diff(
        FILE,
        &after,
        &[
            "  docs:   { uses: jev, when: { files: \"docs/**\" } }",
            "    when:",
            "      or:",
            "        - review",
            "        - and: [desc, { not: [review] }]",
            "  fix:    { uses: fix, needs: [gate] }   # agent defaults to claude",
            "  - or: [review, human]",
            "  - docs: [pass, skipped]",
        ],
        &[
            "      - codex",
            "    when: { or: [review, codex] }",
            "  fix:    { uses: fix, needs: [gate], with: { agent: codex } }   # agent defaults to claude",
            "  codex: { uses: codex, needs: [ci], with: { model: gpt-5 } }",
            "  - { or: [review, codex, human] }",
            "fix_rounds: 4",
        ],
    );
}

// Errors.

#[test]
fn refuses_edits_to_nodes_that_are_not_there() {
    let cases = [
        (
            Edit::RemoveStep { id: "nope".into() },
            EditError::UnknownStep("nope".into()),
        ),
        (
            Edit::AddStep {
                id: "ci".into(),
                step: object(json!({ "uses": "ci" })),
            },
            EditError::StepExists("ci".into()),
        ),
        (
            Edit::RemoveNeed {
                step: "ci".into(),
                need: "x".into(),
            },
            EditError::UnknownNeed {
                step: "ci".into(),
                need: "x".into(),
            },
        ),
        (
            Edit::RemoveGateTerm { index: 4 },
            EditError::UnknownGateTerm(4),
        ),
        (
            Edit::SetGateTerm {
                index: 9,
                term: json!("ci"),
            },
            EditError::UnknownGateTerm(9),
        ),
    ];
    for (edit, error) in cases {
        assert_eq!(apply_edits(FILE, &[edit]), Err(error));
    }
}

// Helpers.

fn edit(text: &str, edits: &[Edit]) -> String {
    apply_edits(text, edits).unwrap_or_else(|e| panic!("{e}"))
}

fn object(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("not an object"),
    }
}

fn gate(pipeline: &Pipeline) -> Vec<String> {
    pipeline
        .gate_terms()
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Asserts that `after` is `before` with exactly the `removed` lines taken
/// out and the `added` lines put in, each in order; every other line is
/// byte-identical and in place.
#[track_caller]
fn assert_diff(before: &str, after: &str, removed: &[&str], added: &[&str]) {
    let (got_removed, got_added) = line_diff(before, after);
    assert!(
        got_removed == removed && got_added == added,
        "removed {got_removed:#?}\nadded {got_added:#?}\nin\n{after}"
    );
}

/// The lines only in `a` and the lines only in `b`, by longest common
/// subsequence.
fn line_diff<'a>(a: &'a str, b: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
    // `split` rather than `lines`, so a lost final newline shows up as a
    // changed line.
    let a: Vec<&str> = a.split('\n').collect();
    let b: Vec<&str> = b.split('\n').collect();
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            added.push(b[j]);
            j += 1;
        } else {
            removed.push(a[i]);
            i += 1;
        }
    }
    (removed, added)
}

/// Asserts that the loader reads `text` as the same Pipeline as `intended`.
#[track_caller]
fn assert_loads_as(text: &str, intended: &str) {
    assert_eq!(
        summary(&load_ok(text)),
        summary(&load_ok(intended)),
        "\n{text}"
    );
}

/// What the loader read, in a form two Pipelines can be compared by.
fn summary(pipeline: &Pipeline) -> String {
    let mut out = format!("fix_rounds: {}\n", pipeline.fix_rounds());
    for step in pipeline.steps() {
        out += &format!(
            "{}: {:?} {} {:?} {} {:?} {:?} {:?}\n",
            step.id,
            step.uses,
            Value::Object(step.config.clone()),
            step.needs,
            step.condition(),
            step.when.is_some(),
            step.timeout,
            step.stall_after,
        );
    }
    out + &format!("gate: {:?}\n", gate(pipeline))
}
