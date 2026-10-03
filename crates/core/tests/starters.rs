//! Starters: the four ready-made Pipelines onboarding offers. Picking one
//! turns into edits on the draft, and the result must load, with the Steps
//! and Gate the onboarding prototype's table lists.

mod support;

use slopwatch_core::{
    EMPTY_PIPELINE, LoadError, Outline, PRESETS, STARTERS, Starter, Workspace, check, load,
    try_edits,
};
use support::TestResolver;

/// The built-in Plugins and a fresh Library: what a Starter resolves
/// against once every built-in Plugin ships.
fn everything() -> TestResolver {
    PRESETS
        .iter()
        .fold(TestResolver::default(), |resolver, preset| {
            resolver.with_library(preset.name, preset.text)
        })
}

/// Picks `starter` on a draft holding `text`, the way the daemon applies
/// it, and returns the file it makes.
fn pick(text: &str, starter: &Starter) -> String {
    let edits = Outline::parse(text).unwrap().use_starter(starter);
    try_edits(text, &edits, &everything()).unwrap_or_else(|refusal| panic!("refused: {refusal}"))
}

fn starter(key: &str) -> &'static Starter {
    Starter::named(key).unwrap_or_else(|| panic!("no Starter `{key}`"))
}

/// Each Step's id and the Plugin it runs, in file order.
fn steps(text: &str) -> Vec<(String, String)> {
    let pipeline = load(text, &everything()).unwrap_or_else(|errors| panic!("{errors:#?}"));
    Outline::parse(text)
        .unwrap()
        .steps()
        .iter()
        .map(|step| {
            let plugin = pipeline.step(&step.id).unwrap().plugin.clone();
            (step.id.clone(), plugin)
        })
        .collect()
}

fn gate(text: &str) -> Vec<String> {
    let pipeline = load(text, &everything()).unwrap();
    pipeline
        .gate_terms()
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(id, plugin)| ((*id).to_owned(), (*plugin).to_owned()))
        .collect()
}

#[test]
fn four_starters_ship_in_the_order_onboarding_offers_them() {
    let names: Vec<&str> = STARTERS.iter().map(|starter| starter.name).collect();

    assert_eq!(
        names,
        [
            "Review and fix",
            "Ask me, then merge",
            "Hands off",
            "Just CI"
        ]
    );
    for starter in STARTERS {
        assert!(!starter.blurb.is_empty(), "{}", starter.key);
    }
}

#[test]
fn review_and_fix_checks_judges_reviews_and_fixes_and_leaves_merging_to_you() {
    let text = pick(EMPTY_PIPELINE, starter("review-and-fix"));

    assert_eq!(
        steps(&text),
        pairs(&[
            ("ci", "ci"),
            ("desc-matches-diff", "jev"),
            ("claude-review", "claude"),
            ("claude-fix", "fix"),
        ])
    );
    assert_eq!(gate(&text), ["ci", "desc-matches-diff", "claude-review"]);
    let pipeline = load(&text, &everything()).unwrap();
    assert_eq!(pipeline.step("claude-review").unwrap().needs, ["ci"]);
    let fix = pipeline.step("claude-fix").unwrap();
    assert!(fix.is_write());
    assert_eq!(fix.condition().to_string(), "{not: [gate]}");
}

#[test]
fn ask_me_then_merge_asks_once_everything_else_passed_then_merges() {
    let text = pick(EMPTY_PIPELINE, starter("ask-then-merge"));

    assert_eq!(
        steps(&text),
        pairs(&[
            ("ci", "ci"),
            ("desc-matches-diff", "jev"),
            ("resolves-issue", "jev"),
            ("claude-review", "claude"),
            ("ship-it", "human"),
            ("merge", "merge"),
            ("claude-fix", "fix"),
        ])
    );
    assert_eq!(
        gate(&text),
        [
            "ci",
            "desc-matches-diff",
            "{resolves-issue: [pass, skipped]}",
            "claude-review",
            "ship-it",
        ]
    );
    let pipeline = load(&text, &everything()).unwrap();
    let ship_it = pipeline.step("ship-it").unwrap();
    assert_eq!(ship_it.config["prompt"], "Ship it?");
    // A PR that links no issue skips `resolves-issue`, and that mustn't
    // skip the question too.
    assert_eq!(
        ship_it.condition().to_string(),
        "[ci, desc-matches-diff, {resolves-issue: [pass, skipped]}, claude-review]"
    );
    assert!(pipeline.step("merge").unwrap().is_merge());
}

#[test]
fn hands_off_adds_codex_as_an_advisory_second_opinion_and_merges() {
    let text = pick(EMPTY_PIPELINE, starter("hands-off"));

    assert_eq!(
        steps(&text),
        pairs(&[
            ("ci", "ci"),
            ("desc-matches-diff", "jev"),
            ("resolves-issue", "jev"),
            ("claude-review", "claude"),
            ("codex-review", "codex"),
            ("claude-fix", "fix"),
            ("merge", "merge"),
        ])
    );
    assert_eq!(
        gate(&text),
        [
            "ci",
            "desc-matches-diff",
            "{resolves-issue: [pass, skipped]}",
            "claude-review",
        ]
    );
    let pipeline = load(&text, &everything()).unwrap();
    assert_eq!(pipeline.step("codex-review").unwrap().needs, ["ci"]);
    // Codex only advises, but its Findings still reach Fix.
    assert_eq!(
        pipeline.step("claude-fix").unwrap().needs,
        ["gate", "codex-review"]
    );
}

#[test]
fn just_ci_puts_ci_in_the_gate() {
    let text = pick(EMPTY_PIPELINE, starter("just-ci"));

    assert_eq!(steps(&text), pairs(&[("ci", "ci")]));
    assert_eq!(gate(&text), ["ci"]);
}

#[test]
fn resolves_issue_skips_itself_on_a_pr_that_links_no_issue() {
    for key in ["ask-then-merge", "hands-off"] {
        let text = pick(EMPTY_PIPELINE, starter(key));
        let pipeline = load(&text, &everything()).unwrap();

        assert_eq!(
            pipeline
                .step("resolves-issue")
                .unwrap()
                .condition()
                .to_string(),
            "{linked_issue: true}",
            "{key}"
        );
    }
}

#[test]
fn picking_a_starter_replaces_what_the_draft_had() {
    let mine = "version: 1\nsteps:\n  lint: { uses: ci }\n  ci: { uses: jev, needs: [lint] }\ngate: [lint, ci]\n";

    let text = pick(mine, starter("just-ci"));

    assert_eq!(steps(&text), pairs(&[("ci", "ci")]));
    assert_eq!(gate(&text), ["ci"]);
    assert_eq!(
        pick(&text, starter("review-and-fix")),
        pick(EMPTY_PIPELINE, starter("review-and-fix"))
    );
}

#[test]
fn a_starter_on_a_machine_without_its_plugins_lacks_only_those() {
    // Before the review and fix Plugins ship, and with the Library's
    // `codex-review` deleted.
    let mut resolver = everything();
    resolver.plugins.remove("claude");
    resolver.plugins.remove("fix");
    resolver.library.remove("codex-review");

    for starter in STARTERS {
        let edits = Outline::parse(EMPTY_PIPELINE).unwrap().use_starter(starter);
        let text = slopwatch_core::apply_edits(EMPTY_PIPELINE, &edits).unwrap();
        let errors = check(&text, &resolver);

        assert_eq!(
            errors.is_empty(),
            starter.key == "just-ci",
            "{}",
            starter.key
        );
        assert!(
            errors.iter().all(LoadError::is_about_this_machine),
            "{}: {errors:#?}",
            starter.key
        );
    }
}

#[test]
fn every_starters_write_step_runs_after_the_gate() {
    for starter in STARTERS {
        let text = pick(EMPTY_PIPELINE, starter);
        let pipeline = load(&text, &everything()).unwrap();
        for step in pipeline.steps() {
            assert!(
                step.workspace != Workspace::Write || step.needs_gate(),
                "{}: a write Step runs after the Gate",
                starter.key
            );
        }
    }
}
