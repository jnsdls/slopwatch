mod support;

use slopwatch_core::{PRESETS, Uses, check_library_step, is_library_step_name, load};
use support::{TestResolver, load_err_with};

fn with_presets() -> TestResolver {
    PRESETS
        .iter()
        .fold(TestResolver::default(), |resolver, preset| {
            resolver.with_library(preset.name, preset.text)
        })
}

#[test]
fn the_five_presets_ship() {
    let mut names: Vec<_> = PRESETS.iter().map(|preset| preset.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "claude-fix",
            "claude-review",
            "codex-review",
            "desc-matches-diff",
            "resolves-issue",
        ]
    );
}

#[test]
fn every_preset_is_a_valid_library_step() {
    for preset in PRESETS {
        assert_eq!(check_library_step(preset.text), Ok(()), "{}", preset.name);
    }
}

/// The Hands off Starter from the onboarding prototype: every preset, the
/// review and judge Steps in the Gate, Codex advisory, Fix after the Gate.
#[test]
fn a_pipeline_of_every_preset_loads() {
    let pipeline = load(
        r#"
version: 1
steps:
  ci:     { uses: ci }
  desc:   { uses: lib/desc-matches-diff }
  issue:  { uses: lib/resolves-issue }
  claude: { uses: lib/claude-review, needs: [ci] }
  codex:  { uses: lib/codex-review, needs: [ci] }
  fix:    { uses: lib/claude-fix, needs: [gate, codex] }
  merge:  { uses: merge, needs: [gate] }
gate:
  - ci
  - desc
  - issue: [pass, skipped]
  - claude
"#,
        &with_presets(),
    )
    .unwrap();

    let plugin = |id: &str| pipeline.step(id).unwrap().plugin.as_str();
    assert_eq!(plugin("desc"), "jev");
    assert_eq!(plugin("issue"), "jev");
    assert_eq!(plugin("claude"), "claude");
    assert_eq!(plugin("codex"), "codex");
    assert_eq!(plugin("fix"), "fix");
    let fix = pipeline.step("fix").unwrap();
    assert!(fix.is_write());
    assert_eq!(fix.uses, Uses::Library("claude-fix".into()));
    assert_eq!(fix.config["agent"], "claude");
    for id in ["claude", "codex", "fix"] {
        assert_eq!(pipeline.step(id).unwrap().config["auth"], "subscription");
    }
}

#[test]
fn a_library_step_must_name_a_plugin() {
    assert_eq!(
        check_library_step("uses: lib/other\n"),
        Err("it uses `lib/other`, but a Library Step must use a Plugin".to_owned())
    );
}

#[test]
fn a_library_step_has_no_needs_or_condition() {
    let error = check_library_step("uses: ci\nneeds: [a]\n").unwrap_err();
    assert!(error.contains("unknown field `needs`"), "{error}");
    let error = check_library_step("uses: ci\nwhen: gate\n").unwrap_err();
    assert!(error.contains("unknown field `when`"), "{error}");
}

#[test]
fn a_library_step_writes_durations_the_pipeline_way() {
    assert_eq!(check_library_step("uses: ci\ntimeout: 30m\n"), Ok(()));
    assert_eq!(
        check_library_step("uses: ci\nstall_after: soon\n"),
        Err("it sets `stall_after: soon`; write a duration such as 90s, 30m or 2h".to_owned())
    );
}

#[test]
fn a_library_step_name_is_one_plain_path_segment() {
    for good in ["claude-review", "my_step2", "2fa"] {
        assert!(is_library_step_name(good), "{good}");
    }
    for bad in [
        "",
        ".hidden",
        "-x",
        "_x",
        "Claude-Review",
        "a/b",
        "..",
        "../x",
        "a b",
        "a.yml",
    ] {
        assert!(!is_library_step_name(bad), "{bad}");
    }
}

#[test]
fn a_reference_that_isnt_a_name_never_reaches_the_resolver() {
    let resolver = TestResolver::default().with_library("../secret", "uses: ci\n");
    let errors = load_err_with(
        "version: 1\nsteps:\n  r: { uses: lib/../secret }\ngate: [r]\n",
        &resolver,
    );
    assert_eq!(
        errors,
        ["Step `r` uses Library Step `../secret`, which doesn't exist"]
    );
}
