//! Made-up PRs, Runs and Inbox entries for working on the GUI without a
//! daemon or GitHub: `SLOPWATCH_DEMO=1 cargo run -p slopwatch-client`. Dev
//! builds only; a release build doesn't compile this module.
//!
//! [`run`] stands in for [`link::run`]: it reports a connection, the
//! fixture's topics, and an answer to each command, so every screen has
//! something on it. Commands change nothing, except that a Starter picked
//! in the tour fills the draft.
//!
//! The fixture is [`prs`], [`inbox`] and the Pipelines in [`PIPELINES`].
//! Add a PR or a Run there to put it on screen. [`LONG_REPO`] holds the
//! stress cases: a 200-character title, a 30-Step Pipeline, 30 Runs on one
//! PR and a 20-PR Stack, so every screen can be checked for overflow.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Receiver;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use slopwatch_core::{
    EMPTY_PIPELINE, EndReason, Expr, GateRole, GateState, Outline, PRESETS, Pipeline, PluginInfo,
    Resolver, STARTERS, Verdict, Workspace, flow_style, library_step_plugin, load,
};
use slopwatch_protocol::pipeline::{DraftBase, DraftStep, PaletteItem, PipelineDraft, PipelinePr};
use slopwatch_protocol::step::{EffectKind, Finding, Outputs, Severity, Usage};
use slopwatch_protocol::{
    Cause, Cli, CliListing, CliSettings, CliStatus, Command, DaemonSettings, EntryId, GateTerm,
    Grant, Inbox, InboxEntry, InboxUpdate, LibraryStep, LogLevel, LogRecord, LogSource,
    PluginListing, PluginSettings, PollState, PrRef, PrStatus, PullRequest, Reply, RepoName,
    RequestId, Response, ResponseBody, RunEvent, RunId, RunSummary, RunView, Scope, SecretInfo,
    StackParent, StackPlace, StepInfo, StepLogPage, StepStatus, StripMark, Topic, TopicUpdate,
    WatchedPrs, WatchedPrsDelta, WatchedPrsUpdate,
};

use crate::link::{LinkEvent, LinkState};

/// Set to `1` to run on the fixture instead of a daemon.
pub const ENV: &str = "SLOPWATCH_DEMO";

pub fn enabled() -> bool {
    std::env::var(ENV).is_ok_and(|value| value == "1")
}

/// Each demo repo's Pipeline on its default branch.
const PIPELINES: &[(&str, &str)] = &[
    (
        "acme/api",
        "version: 1
steps:
  ci: { uses: ci }
  scope-matches-issue: { uses: lib/resolves-issue }
  claude-review: { uses: lib/claude-review, needs: [ci] }
  codex-review: { uses: lib/codex-review, needs: [ci] }
  approve-rollout: { uses: human, needs: [claude-review, scope-matches-issue] }
  merge: { uses: merge, needs: [gate] }
  claude-fix: { uses: lib/claude-fix, needs: [gate] }
gate: [ci, scope-matches-issue, claude-review, approve-rollout]
",
    ),
    (
        "acme/web",
        "version: 1
steps:
  ci: { uses: ci }
  ui-has-screenshots: { uses: jev, when: { files: [\"app/**\"] } }
  claude-review: { uses: lib/claude-review, needs: [ci] }
  merge: { uses: merge, needs: [gate] }
  claude-fix: { uses: lib/claude-fix, needs: [gate] }
gate: [ci, { ui-has-screenshots: [pass, skipped] }, claude-review]
",
    ),
    (
        "jnsdls/slopwatch",
        "version: 1
steps:
  ci: { uses: ci }
  claude-review: { uses: lib/claude-review, needs: [ci] }
  codex-review: { uses: lib/codex-review, needs: [ci] }
gate: [ci, claude-review]
",
    ),
];

/// A repo with a long name, whose Pipeline has 30 Steps: [`long_pipeline`].
const LONG_REPO: &str = "acme-platform-engineering/internal-developer-experience-monorepo";

/// A PR title 200 characters long.
const LONG_TITLE: &str = "Migrate the webhook ingest, billing reconciliation and audit-log \
                          exporters from the legacy v1 queue onto the partitioned event bus, \
                          with dual writes, a resumable backfill job and a kill switch per org";

/// The 25 checks in [`long_pipeline`], beside its CI, review, Human Step,
/// Merge and Fix.
const LONG_CHECKS: [&str; 25] = [
    "scope-matches-issue",
    "description-matches-diff",
    "migrations-are-reversible-and-idempotent-on-a-copy-of-production",
    "no-new-public-api-without-docs",
    "feature-flags-default-off",
    "changelog-entry",
    "license-headers",
    "no-secrets-in-diff",
    "bundle-size-budget",
    "accessibility-labels",
    "i18n-strings-extracted",
    "error-messages-are-actionable",
    "metrics-have-owners",
    "dashboards-updated",
    "runbook-updated",
    "rollback-plan",
    "load-test-p99-under-budget",
    "dependency-licences",
    "sbom-updated",
    "tests-cover-new-branches",
    "no-todo-without-a-ticket",
    "api-backward-compatible",
    "schema-versioned",
    "cache-keys-namespaced",
    "retry-policy-bounded",
];

/// The long repo's Human Step.
const LONG_HUMAN: &str = "approve-the-production-rollout-plan-with-the-on-call-sre";

/// [`LONG_REPO`]'s Pipeline: CI, 25 checks and a review before the Gate, a
/// Human Step after the review, then Merge and Fix. Ten of them in the Gate.
fn long_pipeline() -> String {
    let mut text = "version: 1\nsteps:\n  ci: { uses: ci }\n".to_owned();
    for check in LONG_CHECKS {
        text += &format!("  {check}: {{ uses: jev, needs: [ci] }}\n");
    }
    text += "  claude-review: { uses: lib/claude-review, needs: [ci] }\n";
    text += &format!("  {LONG_HUMAN}: {{ uses: human, needs: [claude-review] }}\n");
    text += "  merge: { uses: merge, needs: [gate] }\n";
    text += "  claude-fix: { uses: lib/claude-fix, needs: [gate] }\n";
    let gated = ["ci", "claude-review", LONG_HUMAN]
        .into_iter()
        .chain(LONG_CHECKS.into_iter().take(7));
    text += &format!("gate: [{}]\n", gated.collect::<Vec<_>>().join(", "));
    text
}

/// Every demo repo's Pipeline text.
fn pipelines() -> Vec<(&'static str, String)> {
    PIPELINES
        .iter()
        .map(|(repo, text)| (*repo, (*text).to_owned()))
        .chain([(LONG_REPO, long_pipeline())])
        .collect()
}

/// `repo`'s Pipeline text, if it's a demo repo.
fn pipeline_text(repo: &RepoName) -> Option<String> {
    pipelines()
        .into_iter()
        .find(|(name, _)| repo_name(name) == *repo)
        .map(|(_, text)| text)
}

/// Repos the picker offers, which have no Pipeline yet: more than fit,
/// one with a long name.
const AVAILABLE: &[&str] = &[
    "jnsdls/dotfiles",
    "typesafe-ai/web",
    "acme-platform-engineering/observability-gateway-terraform-modules-for-every-region",
    "acme/android",
    "acme/billing",
    "acme/data-pipelines",
    "acme/design-system",
    "acme/docs",
    "acme/infra",
    "acme/ios",
    "acme/notifications",
    "acme/search",
];

/// Reports what a daemon would until `report` returns false or the window
/// stops sending commands.
pub fn run(commands: &Receiver<(RequestId, Command)>, mut report: impl FnMut(LinkEvent) -> bool) {
    let mut fixture = Fixture::new();
    let mut opening = vec![
        LinkEvent::State(LinkState::Connected {
            daemon_build_id: "demo".into(),
        }),
        LinkEvent::Topic(TopicUpdate::WatchedPrs {
            seq: 1,
            update: WatchedPrsUpdate::Snapshot(fixture.watched()),
        }),
        LinkEvent::Topic(TopicUpdate::Inbox {
            seq: 1,
            update: InboxUpdate::Snapshot(Inbox {
                entries: inbox(&fixture.now),
            }),
        }),
    ];
    for event in opening.drain(..) {
        if !report(event) {
            return;
        }
    }
    while let Ok((id, command)) = commands.recv() {
        let (events, reply) = fixture.answer(command);
        for event in events {
            if !report(LinkEvent::Topic(event)) {
                return;
            }
        }
        let response = Response {
            id,
            result: ResponseBody::Ok(reply),
        };
        if !report(LinkEvent::Response(response)) {
            return;
        }
    }
}

/// When the fixture is, in milliseconds since the Unix epoch, so its
/// times read as minutes ago.
#[derive(Debug, Clone, Copy)]
struct Now(i64);

impl Now {
    fn minutes_ago(self, minutes: f64) -> i64 {
        self.0 - (minutes * 60_000.) as i64
    }
}

struct Fixture {
    now: Now,
    prs: Vec<DemoPr>,
    /// Each repo's draft as the editor last got it.
    drafts: HashMap<RepoName, PipelineDraft>,
    /// The last `watched_prs` sequence number sent.
    seq: u64,
}

struct DemoPr {
    pr: PullRequest,
    /// Newest first.
    runs: Vec<DemoRun>,
}

impl Fixture {
    fn new() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64);
        let now = Now(millis);
        let mut prs = prs(now);
        for demo in &mut prs {
            for run in &mut demo.runs {
                if let Some((_, RunEvent::Started { repo, number, .. })) = run.events.first_mut() {
                    *repo = demo.pr.repo.clone();
                    *number = demo.pr.number;
                }
            }
        }
        Fixture {
            now,
            prs,
            drafts: HashMap::new(),
            seq: 1,
        }
    }

    fn watched(&self) -> WatchedPrs {
        let mut prs: Vec<PullRequest> = self
            .prs
            .iter()
            .map(|demo| {
                let mut pr = demo.pr.clone();
                pr.runs = demo.runs.iter().map(DemoRun::summary).collect();
                // Only the newest Run carries its strip, as from the daemon.
                for older in pr.runs.iter_mut().skip(1) {
                    older.strip.clear();
                }
                pr
            })
            .collect();
        prs.sort_by(|a, b| (&a.repo, a.number).cmp(&(&b.repo, b.number)));
        WatchedPrs {
            repos: pipelines()
                .iter()
                .map(|(repo, _)| repo_name(repo))
                .collect(),
            prs,
            poll: PollState::Online,
            storage: None,
        }
    }

    /// The topic updates `command` brings, and the reply to it.
    fn answer(&mut self, command: Command) -> (Vec<TopicUpdate>, Reply) {
        match command {
            Command::Subscribe {
                topic: Topic::Run(id),
                since,
            } => {
                let run = self.prs.iter().flat_map(|pr| &pr.runs).find(|r| r.id == id);
                let updates = run.map_or_else(Vec::new, |run| run.updates(since.unwrap_or(0)));
                (updates, Reply::Done)
            }
            Command::Subscribe {
                topic: Topic::StepLog(key),
                ..
            } => {
                let records = log(&self.now);
                (vec![TopicUpdate::StepLog { key, records }], Reply::Done)
            }
            Command::ReadStepLog { key, page, filter } => {
                let page = StepLogPage {
                    key,
                    page,
                    filter,
                    records: log(&self.now),
                    more_before: false,
                    more_after: false,
                    truncated: None,
                };
                (Vec::new(), Reply::StepLog(page))
            }
            Command::Subscribe {
                topic: Topic::Pipeline(repo),
                ..
            } => {
                let draft = self.drafts.entry(repo.clone()).or_insert_with(|| {
                    let text = pipeline_text(&repo);
                    let committed = text.is_some();
                    draft(&repo, text.as_deref().unwrap_or(EMPTY_PIPELINE), committed)
                });
                (vec![pipeline_update(draft)], Reply::Done)
            }
            Command::ApplyStarter { repo, starter, .. } => {
                let Some(starter) = STARTERS.iter().find(|each| each.key == starter) else {
                    return (Vec::new(), Reply::Done);
                };
                let mut picked = draft(&repo, starter.text, false);
                picked.edits = Outline::parse(EMPTY_PIPELINE)
                    .expect("the empty Pipeline parses")
                    .use_starter(starter);
                let update = pipeline_update(&picked);
                self.drafts.insert(repo, picked);
                (vec![update], Reply::Done)
            }
            Command::PublishPipeline { repo, .. } => {
                let Some(draft) = self.drafts.get_mut(&repo) else {
                    return (Vec::new(), Reply::Done);
                };
                draft.published = Some(PipelinePr {
                    number: 1,
                    url: format!("https://github.com/{repo}/pull/1"),
                    head: "5107e1d".into(),
                });
                (vec![pipeline_update(draft)], Reply::Done)
            }
            Command::AddRepo { repo } => {
                // The new repo comes with one open PR, unwatched, for the
                // tour's Watch Stop.
                let mut added = pr(&repo.to_string(), 7, "Add zsh completions", "77aa77a");
                added.status = PrStatus::NotWatched;
                let updates = vec![
                    self.delta(WatchedPrsDelta::RepoAdded { repo }),
                    self.delta(WatchedPrsDelta::PrChanged { pr: added }),
                ];
                (updates, Reply::Done)
            }
            // Only the tour's new repo has PRs that change. They wait for
            // its Pipeline, which hasn't landed.
            Command::Watch { repo, number } => {
                let update = self.tour_pr(&repo, number, PrStatus::Waiting);
                (vec![update], Reply::Done)
            }
            Command::Unwatch { repo, number } => {
                let update = self.tour_pr(&repo, number, PrStatus::NotWatched);
                (vec![update], Reply::Done)
            }
            Command::ListAvailableRepos => {
                let repos = AVAILABLE.iter().map(|repo| repo_name(repo)).collect();
                (Vec::new(), Reply::AvailableRepos { repos })
            }
            Command::ListLibrarySteps => {
                let long = LibraryStep {
                    name: format!("{LONG_PLUGIN}-and-idempotent-on-a-production-snapshot"),
                    text: format!("uses: {LONG_PLUGIN}\n"),
                    problem: Some(format!(
                        "Plugin `{LONG_PLUGIN}` has no Approval, so a Pipeline that uses this \
                         Step won't load until it's approved under Plugins."
                    )),
                };
                let steps = PRESETS
                    .iter()
                    .map(|preset| LibraryStep {
                        name: preset.name.into(),
                        text: preset.text.into(),
                        problem: None,
                    })
                    .chain([long])
                    .collect();
                (Vec::new(), Reply::LibrarySteps { steps })
            }
            Command::ListSecrets => (Vec::new(), secrets()),
            Command::ListPlugins => (Vec::new(), plugins()),
            Command::GetSettings => (
                Vec::new(),
                Reply::Settings {
                    settings: DaemonSettings::default(),
                    spent_today: slopwatch_protocol::Cents(412),
                },
            ),
            Command::ListClis => (Vec::new(), clis()),
            _ => (Vec::new(), Reply::Done),
        }
    }
}

impl Fixture {
    /// The tour's new repo's one PR, as a delta.
    fn tour_pr(&mut self, repo: &RepoName, number: u64, status: PrStatus) -> TopicUpdate {
        let mut changed = pr(&repo.to_string(), number, "Add zsh completions", "77aa77a");
        changed.status = status;
        self.delta(WatchedPrsDelta::PrChanged { pr: changed })
    }

    /// A `watched_prs` delta, with the next sequence number.
    fn delta(&mut self, delta: WatchedPrsDelta) -> TopicUpdate {
        self.seq += 1;
        TopicUpdate::WatchedPrs {
            seq: self.seq,
            update: WatchedPrsUpdate::Delta(delta),
        }
    }
}

fn pipeline_update(draft: &PipelineDraft) -> TopicUpdate {
    TopicUpdate::Pipeline {
        draft: Box::new(draft.clone()),
    }
}

/// The developer's open PRs, by repo.
fn prs(now: Now) -> Vec<DemoPr> {
    let api = DemoPipeline::of("acme/api");
    let web = DemoPipeline::of("acme/web");
    let slopwatch = DemoPipeline::of("jnsdls/slopwatch");
    let review = |findings| Outputs {
        findings,
        ..Outputs::default()
    };
    let note = |text: &str| Outputs {
        note: Some(text.into()),
        ..Outputs::default()
    };
    vec![
        DemoPr {
            pr: pr("acme/api", 482, "Rate-limit the webhook ingest", "a1b2c3d"),
            runs: vec![
                api.run(now, 3, "a1b2c3d", 26.)
                    .settle("ci", 26., 17., Verdict::Pass, Outputs::default())
                    .settle("scope-matches-issue", 26., 25.8, Verdict::Pass, note(
                        "Diff touches only ingest and config, as issue #311 asks.",
                    ))
                    .cost("scope-matches-issue", 0.002)
                    .settle("claude-review", 17., 11., Verdict::Pass, review(vec![
                        finding(Severity::Info, "Token bucket refill uses wall clock; monotonic would avoid skew.", Some(("src/ingest/limit.rs", 88))),
                        finding(Severity::Info, "Magic number 512 for burst size.", Some(("src/config.rs", 41))),
                    ]))
                    .cost("claude-review", 0.38)
                    .settle("codex-review", 17., 9., Verdict::Inconclusive, review(vec![
                        finding(Severity::Warning, "Could not determine whether 429s are retried by senders.", None),
                    ]))
                    .start("approve-rollout", 11.),
                api.run(now, 2, "9f8e7d6", 58.)
                    .settle("ci", 58., 49., Verdict::Pass, Outputs::default())
                    .settle("scope-matches-issue", 58., 57.8, Verdict::Pass, Outputs::default())
                    .settle("claude-review", 49., 43., Verdict::Fail, review(vec![
                        finding(Severity::Error, "Limiter keyed by IP, but ingest sits behind the LB: every tenant shares one bucket.", Some(("src/ingest/limit.rs", 30))),
                    ]))
                    .cost("claude-review", 0.41)
                    .settle("codex-review", 49., 42., Verdict::Pass, Outputs::default())
                    .skip("approve-rollout", 43., "claude-review didn't pass")
                    .gate(GateState::Fail, 43.)
                    .skip("merge", 43., "Gate failed")
                    .settle("claude-fix", 43., 29., Verdict::Pass, note("Keyed the limiter by tenant id; added a test."))
                    .cost("claude-fix", 0.92)
                    .end(EndReason::Pushed, 29.),
                api.run(now, 1, "5a4b3c2", 95.)
                    .settle("ci", 95., 87., Verdict::Fail, review(vec![
                        finding(Severity::Error, "test ingest::limit::burst failed: expected 429, got 200", None),
                    ]))
                    .settle("scope-matches-issue", 95., 94.8, Verdict::Pass, Outputs::default())
                    .skip("claude-review", 87., "CI didn't pass")
                    .skip("codex-review", 87., "CI didn't pass")
                    .skip("approve-rollout", 87., "claude-review didn't pass")
                    .gate(GateState::Fail, 87.)
                    .skip("merge", 87., "Gate failed")
                    .settle("claude-fix", 87., 61., Verdict::Pass, note("Fixed off-by-one in burst accounting."))
                    .end(EndReason::Pushed, 61.),
            ],
        },
        DemoPr {
            pr: pr("acme/api", 479, "Drop legacy v1 auth", "c0ffee1"),
            runs: vec![api
                .run(now, 4, "c0ffee1", 12.)
                .start("ci", 12.)
                .settle("scope-matches-issue", 12., 11.8, Verdict::Pass, Outputs::default())],
        },
        DemoPr {
            pr: pr("acme/web", 1203, "Settings page redesign", "d4d4d4d"),
            runs: vec![
                web.run(now, 8, "d4d4d4d", 35.)
                    .settle("ci", 35., 28., Verdict::Pass, Outputs::default())
                    .settle("ui-has-screenshots", 35., 34.8, Verdict::Pass, Outputs::default())
                    .settle("claude-review", 28., 21., Verdict::Fail, review(vec![
                        finding(Severity::Error, "Form loses unsaved changes on tab switch.", Some(("app/settings/page.tsx", 212))),
                        finding(Severity::Warning, "Toggle has no accessible label.", Some(("app/settings/Toggle.tsx", 14))),
                    ]))
                    .cost("claude-review", 0.47)
                    .gate(GateState::Fail, 21.)
                    .skip("merge", 21., "Gate failed")
                    .skip("claude-fix", 21., "round cap reached")
                    .end(EndReason::NotShippable, 19.),
                web.run(now, 7, "c3c3c3c", 62.)
                    .settle("ci", 62., 55., Verdict::Pass, Outputs::default())
                    .settle("ui-has-screenshots", 62., 61.8, Verdict::Pass, Outputs::default())
                    .settle("claude-review", 55., 48., Verdict::Fail, Outputs::default())
                    .gate(GateState::Fail, 48.)
                    .skip("merge", 48., "Gate failed")
                    .settle("claude-fix", 48., 38., Verdict::Pass, Outputs::default())
                    .end(EndReason::Pushed, 38.),
                web.run(now, 6, "a1a1a1a", 130.)
                    .settle("ci", 130., 123., Verdict::Pass, Outputs::default())
                    .settle("ui-has-screenshots", 130., 129.8, Verdict::Fail, review(vec![
                        finding(Severity::Warning, "PR changes UI but has no screenshots.", None),
                    ]))
                    .start("claude-review", 123.)
                    .cancel_running(92.)
                    .end(EndReason::Superseded, 92.),
            ],
        },
        DemoPr {
            pr: pr("acme/web", 1199, "Fix hydration flicker", "e1e1e1e"),
            runs: vec![web
                .run(now, 9, "e1e1e1e", 24.)
                .settle("ci", 24., 17., Verdict::Pass, Outputs::default())
                .skip("ui-has-screenshots", 24., "Condition false")
                .start("claude-review", 17.)],
        },
        DemoPr {
            pr: stacked(
                pr("jnsdls/slopwatch", 120, "Theme module for the prototype tokens", "f0f0f0f"),
                119,
                "Shared pill, chip and dot components",
            ),
            runs: vec![slopwatch
                .run(now, 14, "f0f0f0f", 6.)
                .start("ci", 6.)],
        },
        DemoPr {
            pr: stacked(
                pr("jnsdls/slopwatch", 119, "Shared pill, chip and dot components", "e0e0e0e"),
                118,
                "Loading states for every async action",
            ),
            runs: vec![slopwatch
                .run(now, 13, "e0e0e0e", 40.)
                .settle("ci", 40., 31., Verdict::Pass, Outputs::default())
                .settle("claude-review", 31., 26., Verdict::Pass, Outputs::default())
                .cost("claude-review", 0.22)
                .settle("codex-review", 31., 25., Verdict::Pass, Outputs::default())
                .gate(GateState::Pass, 26.)
                .end(EndReason::Shippable, 25.)],
        },
        DemoPr {
            pr: unwatched(pr("jnsdls/slopwatch", 121, "Overflow in long titles", "abababa")),
            runs: Vec::new(),
        },
    ]
    .into_iter()
    .chain(long_prs(now))
    .collect()
}

/// A long file path, for Findings.
const LONG_PATH: &str = "services/ingest/src/partitioned_event_bus/consumers/billing_reconciliation/dual_write_coordinator.rs";

/// [`LONG_REPO`]'s PRs: one with a 200-character title and 30 Runs, the
/// newest live with every kind of Step, then a Stack of 20.
fn long_prs(now: Now) -> Vec<DemoPr> {
    let long = DemoPipeline::of(LONG_REPO);
    let review = |findings| Outputs {
        findings,
        ..Outputs::default()
    };
    let note = |text: &str| Outputs {
        note: Some(text.into()),
        ..Outputs::default()
    };
    let mut live = long
        .run(now, 130, "deadbee", 30.)
        .settle("ci", 30., 22., Verdict::Pass, Outputs::default())
        .settle("claude-review", 22., 15., Verdict::Fail, review(vec![
            finding(Severity::Error, "The dual-write coordinator acknowledges the v1 queue before the event bus confirms the write, so a crash between the two loses the event.", Some((LONG_PATH, 1184))),
            finding(Severity::Warning, "Backfill cursor is stored per process, not per partition.", Some(("services/ingest/src/partitioned_event_bus/backfill/resumable_cursor_store_with_lease_renewal.rs", 77))),
            finding(Severity::Info, "Consider naming the kill switch after the tenant setting it reads.", None),
        ]))
        .cost("claude-review", 1.27)
        .settle(LONG_CHECKS[2], 22., 18., Verdict::Error, note(
            "The migration runner exited 137 while restoring the production snapshot into the scratch database: \
             out of memory at /var/folders/xy/scratch-databases/acme-platform-engineering/internal-developer-experience-monorepo/snapshot-2026-10-04.dump",
        ));
    for (index, check) in LONG_CHECKS.iter().enumerate() {
        let start = 22. - index as f64 * 0.3;
        live = match index {
            2 => live,
            1 | 9 => live.settle(check, start, start - 2., Verdict::Fail, review(vec![
                finding(Severity::Warning, "The description promises a kill switch per tenant, but the diff adds one per org.", Some((LONG_PATH, 42))),
            ])),
            5 => live.settle(check, start, start - 1., Verdict::Inconclusive, Outputs::default()),
            20.. => live.start(check, start),
            _ => live.settle(check, start, start - 1.5, Verdict::Pass, Outputs::default()),
        };
    }
    let live = live.start(LONG_HUMAN, 14.);
    let ends = [
        EndReason::Superseded,
        EndReason::Pushed,
        EndReason::NotShippable,
        EndReason::Cancelled,
        EndReason::OverBudget,
    ];
    let mut runs = vec![live];
    for id in (101..130_u64).rev() {
        let minutes = 30. + (130 - id) as f64 * 20.;
        let reason = ends[id as usize % ends.len()];
        runs.push(
            long.run(now, id, "0ldc0de", minutes)
                .settle(
                    "ci",
                    minutes,
                    minutes - 8.,
                    Verdict::Pass,
                    Outputs::default(),
                )
                .end(reason, minutes - 10.),
        );
    }
    let mut prs = vec![DemoPr {
        pr: pr(LONG_REPO, 4242, LONG_TITLE, "deadbee"),
        runs,
    }];
    let part = |number: u64| {
        format!(
            "Event bus migration, part {} of 20: move one more consumer onto the partitioned bus",
            number - 9000
        )
    };
    for number in 9001..=9020 {
        let mut each = pr(LONG_REPO, number, &part(number), "5ac5ac5");
        if number > 9001 {
            each = stacked(each, number - 1, &part(number - 1));
        }
        prs.push(DemoPr {
            pr: each,
            runs: Vec::new(),
        });
    }
    prs
}

/// Every Step's log: a few lines, one of them too long for the pane and
/// one a path with no spaces to wrap at.
fn log(now: &Now) -> Vec<LogRecord> {
    let lines = [
        (LogSource::Log, Some(LogLevel::Info), "Reading the diff against main".to_owned()),
        (LogSource::Stderr, None, format!("warning: unused import in {LONG_PATH}:12")),
        (
            LogSource::Log,
            Some(LogLevel::Warn),
            "Restoring the production snapshot into the scratch database took longer than \
             the five minutes the runbook allows, so the check retried it once with a \
             smaller sample of tenants before it gave up and reported the timing."
                .to_owned(),
        ),
        (
            LogSource::Stderr,
            None,
            "/var/folders/xy/scratch-databases/acme-platform-engineering/internal-developer-experience-monorepo/snapshot-2026-10-04.dump".to_owned(),
        ),
        (LogSource::Log, Some(LogLevel::Info), "Done".to_owned()),
    ];
    lines
        .into_iter()
        .enumerate()
        .map(|(index, (source, level, text))| LogRecord {
            seq: index as u64 + 1,
            ts: now.minutes_ago(5. - index as f64),
            source,
            level,
            text,
        })
        .collect()
}

/// The open Inbox entries, oldest first.
fn inbox(now: &Now) -> Vec<InboxEntry> {
    let seconds = |minutes: f64| now.minutes_ago(minutes) / 1000;
    vec![
        InboxEntry {
            id: EntryId(31),
            scope: Scope::Pr,
            title: "Not shippable".into(),
            reasons: vec![
                "Fix round cap reached (3 of 3).".into(),
                "claude-review still fails.".into(),
            ],
            prs: vec![pr_ref("acme/web", 1203)],
            raised_at: seconds(19.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(32),
            scope: Scope::Human {
                run: RunId(3),
                step: "approve-rollout".into(),
            },
            title: "Check the rate-limit defaults are sane for enterprise tenants.".into(),
            reasons: Vec::new(),
            prs: vec![pr_ref("acme/api", 482)],
            raised_at: seconds(11.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(33),
            scope: Scope::Run {
                run: RunId(9),
                step: Some("claude-review".into()),
            },
            title: "claude-review stalled".into(),
            reasons: vec!["No message for 5 min.".into()],
            prs: vec![pr_ref("acme/web", 1199)],
            raised_at: seconds(4.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(34),
            scope: Scope::Cause {
                cause: Cause::MissingSecret {
                    name: "AI_GATEWAY_API_KEY".into(),
                },
            },
            title: "Missing Secret AI_GATEWAY_API_KEY".into(),
            reasons: vec!["desc-matches-diff needs it to run.".into()],
            prs: vec![pr_ref("acme/api", 479)],
            raised_at: seconds(2.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(40),
            scope: Scope::Human {
                run: RunId(130),
                step: LONG_HUMAN.into(),
            },
            title: "Read the rollout plan in the PR description and confirm the on-call SRE \
                    has signed off: dual writes stay on for two weeks, the backfill runs at \
                    night in batches of ten thousand events, and the kill switch per org is \
                    tested in staging against a copy of the largest tenant before it ships."
                .into(),
            reasons: Vec::new(),
            prs: vec![pr_ref(LONG_REPO, 4242)],
            raised_at: seconds(14.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(41),
            scope: Scope::Run {
                run: RunId(130),
                step: Some(LONG_CHECKS[2].into()),
            },
            title: format!("{} errored", LONG_CHECKS[2]),
            reasons: vec![
                "The migration runner exited 137 while restoring the production snapshot \
                 into the scratch database, which usually means it ran out of memory. Retry \
                 it once the snapshot is smaller, or raise the scratch database's memory in \
                 /etc/acme/scratch-databases/internal-developer-experience-monorepo.toml."
                    .into(),
            ],
            prs: vec![pr_ref(LONG_REPO, 4242)],
            raised_at: seconds(18.),
            budget: None,
            closed: None,
        },
        InboxEntry {
            id: EntryId(42),
            scope: Scope::Cause {
                cause: Cause::MissingSecret {
                    name: LONG_SECRET.into(),
                },
            },
            title: format!("Missing Secret {LONG_SECRET}"),
            reasons: vec![format!("{} needs it to run.", LONG_CHECKS[3])],
            prs: (9001..=9020)
                .map(|number| pr_ref(LONG_REPO, number))
                .collect(),
            raised_at: seconds(1.),
            budget: None,
            closed: None,
        },
    ]
}

/// A Secret with a long name, which a third-party Plugin needs.
const LONG_SECRET: &str = "ACME_INTERNAL_DEVELOPER_PLATFORM_OBSERVABILITY_GATEWAY_API_KEY";

/// A third-party Plugin with a long name, found at a long path.
const LONG_PLUGIN: &str = "verify-database-migrations-are-reversible";

fn secrets() -> Reply {
    Reply::Secrets {
        secrets: vec![
            SecretInfo {
                name: "AI_GATEWAY_API_KEY".into(),
                set_at: None,
                granted_to: vec!["jev".into()],
                optional: false,
            },
            SecretInfo {
                name: "ANTHROPIC_API_KEY".into(),
                set_at: Some(1_790_000_000),
                granted_to: vec!["claude".into(), "fix".into()],
                optional: true,
            },
            SecretInfo {
                name: LONG_SECRET.into(),
                set_at: None,
                granted_to: vec![LONG_PLUGIN.into(), "jev".into()],
                optional: false,
            },
        ],
    }
}

fn plugins() -> Reply {
    let builtin = |name: &str| PluginListing {
        name: name.into(),
        builtin: true,
        path: None,
        version: Some("0.1.0".into()),
        asks: None,
        approved: None,
        approved_at: None,
        problem: None,
        settings: Default::default(),
    };
    let third_party = PluginListing {
        name: LONG_PLUGIN.into(),
        builtin: false,
        path: Some(format!(
            "/Users/jnsdls/Library/Application Support/slopwatch/plugins/{LONG_PLUGIN}/target/release/{LONG_PLUGIN}"
        )),
        version: Some("2.14.0-rc.3+build.20261004".into()),
        asks: Some(Grant {
            workspace: Workspace::Read,
            effects: vec![EffectKind::Comment, EffectKind::Label],
            secrets: vec![LONG_SECRET.into()],
        }),
        approved: None,
        approved_at: None,
        problem: None,
        settings: PluginSettings {
            path: vec![
                "/opt/homebrew/opt/postgresql@17/bin".into(),
                "/Users/jnsdls/code/acme/internal-developer-experience-monorepo/tools/bin".into(),
            ],
            cap: Some(2),
            config_dir: None,
        },
    };
    let plugins = ["ci", "claude", "codex", "fix", "human", "jev", "merge"]
        .into_iter()
        .map(builtin)
        .chain([third_party])
        .collect();
    Reply::Plugins { plugins }
}

fn clis() -> Reply {
    let clis = Cli::ALL
        .into_iter()
        .map(|cli| CliListing {
            cli,
            settings: CliSettings::default(),
            status: CliStatus::default(),
        })
        .collect();
    Reply::Clis { clis }
}

/// A repo's Pipeline, loaded to give its Runs their Steps and Gate.
struct DemoPipeline {
    pipeline: Pipeline,
}

impl DemoPipeline {
    fn of(repo: &str) -> Self {
        let text = pipeline_text(&repo_name(repo)).expect("a demo repo");
        let pipeline = load(&text, &Builtins).unwrap_or_else(|errors| {
            let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
            panic!("the {repo} demo Pipeline loads: {}", errors.join("; "))
        });
        DemoPipeline { pipeline }
    }

    /// A Run that started `minutes` ago. Add its events with the builder.
    fn run(&self, now: Now, id: u64, head_sha: &str, minutes: f64) -> DemoRun {
        let steps = self
            .pipeline
            .ordered_steps()
            .map(|step| StepInfo {
                id: step.id.clone(),
                plugin: step.plugin.clone(),
                needs: step.needs.clone(),
                gated: self.pipeline.gate_reads(&step.id),
                write: step.is_write(),
                condition: step.when.as_ref().map(ToString::to_string),
            })
            .collect();
        let gate_terms: Vec<GateTerm> = self
            .pipeline
            .gate_terms()
            .iter()
            .map(GateTerm::from)
            .collect();
        let gate = self
            .pipeline
            .gate_terms()
            .iter()
            .map(Expr::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut run = DemoRun {
            id: RunId(id),
            head_sha: head_sha.into(),
            now,
            events: Vec::new(),
        };
        run.push(
            minutes,
            RunEvent::Started {
                // The PR's, once the fixture has it.
                repo: RepoName::new("demo", "demo"),
                number: 0,
                head_sha: head_sha.into(),
                base: "main".into(),
                base_sha: "9f2c1e0".into(),
                steps,
                gate: format!("[{gate}]"),
                gate_terms,
            },
        );
        run.push(
            minutes,
            RunEvent::Gate {
                state: GateState::Pending,
            },
        );
        run
    }
}

/// One Run's journal, built event by event.
struct DemoRun {
    id: RunId,
    head_sha: String,
    now: Now,
    /// Each event with when it was journalled.
    events: Vec<(i64, RunEvent)>,
}

impl DemoRun {
    fn push(&mut self, minutes: f64, event: RunEvent) {
        self.events.push((self.now.minutes_ago(minutes), event));
    }

    fn start(mut self, step: &str, at: f64) -> Self {
        let step = step.to_owned();
        self.push(at, RunEvent::StepStarted { step, attempt: 1 });
        self
    }

    fn settle(self, step: &str, from: f64, to: f64, verdict: Verdict, outputs: Outputs) -> Self {
        let mut run = self.start(step, from);
        run.push(
            to,
            RunEvent::StepSettled {
                step: step.to_owned(),
                verdict,
                reason: None,
                outputs,
                reused_from: None,
            },
        );
        run
    }

    fn skip(mut self, step: &str, at: f64, reason: &str) -> Self {
        self.push(
            at,
            RunEvent::StepSettled {
                step: step.to_owned(),
                verdict: Verdict::Skipped,
                reason: Some(reason.into()),
                outputs: Outputs::default(),
                reused_from: None,
            },
        );
        self
    }

    fn cost(mut self, step: &str, usd: f64) -> Self {
        let usage = Usage {
            model: "claude-opus-5-5".into(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            usd: Some(usd),
        };
        let at = self
            .events
            .last()
            .map_or(0., |(ts, _)| (self.now.0 - ts) as f64 / 60_000.);
        self.push(
            at,
            RunEvent::StepUsage {
                step: step.to_owned(),
                usage,
            },
        );
        self
    }

    /// Cancels every Step that's running or hasn't started.
    fn cancel_running(mut self, at: f64) -> Self {
        let view = self.view();
        for step in &view.steps {
            if !matches!(step.status, StepStatus::Settled { .. }) {
                self = self.settle_now(&step.info.id, at, Verdict::Cancelled);
            }
        }
        self
    }

    fn settle_now(mut self, step: &str, at: f64, verdict: Verdict) -> Self {
        self.push(
            at,
            RunEvent::StepSettled {
                step: step.to_owned(),
                verdict,
                reason: None,
                outputs: Outputs::default(),
                reused_from: None,
            },
        );
        self
    }

    fn gate(mut self, state: GateState, at: f64) -> Self {
        self.push(at, RunEvent::Gate { state });
        self
    }

    fn end(mut self, reason: EndReason, at: f64) -> Self {
        self.push(
            at,
            RunEvent::Ended {
                reason,
                waived: false,
            },
        );
        self
    }

    /// The journal from sequence number `since` on, as topic updates.
    fn updates(&self, since: u64) -> Vec<TopicUpdate> {
        self.events
            .iter()
            .enumerate()
            .map(|(index, (ts, event))| (index as u64 + 1, *ts, event))
            .filter(|(seq, ..)| *seq > since)
            .map(|(seq, ts, event)| TopicUpdate::Run {
                id: self.id,
                seq,
                ts,
                event: event.clone(),
            })
            .collect()
    }

    fn view(&self) -> RunView {
        let mut view = RunView::default();
        for (index, (_, event)) in self.events.iter().enumerate() {
            view.apply(index as u64 + 1, event.clone());
        }
        view
    }

    fn summary(&self) -> RunSummary {
        let view = self.view();
        RunSummary {
            id: self.id,
            head_sha: self.head_sha.clone(),
            gate: view.gate.unwrap_or(GateState::Pending),
            end: view.end,
            waived: view.waived,
            strip: strip(&view),
        }
    }
}

/// The Run's Step strip, the way the daemon marks it: the Gate between
/// the Steps it doesn't come after and the Steps it does.
fn strip(view: &RunView) -> Vec<StripMark> {
    let mut after_gate: Vec<&str> = Vec::new();
    let (mut before, mut after) = (Vec::new(), Vec::new());
    for step in &view.steps {
        let mark = match &step.status {
            StepStatus::Pending => StripMark::Pending,
            StepStatus::Running if step.info.plugin == "human" => StripMark::Asking,
            StepStatus::Running => StripMark::Running,
            StepStatus::Settled { verdict, .. } => StripMark::Settled(*verdict),
        };
        let downstream = step
            .info
            .needs
            .iter()
            .any(|need| need == "gate" || after_gate.contains(&need.as_str()));
        if downstream {
            after_gate.push(&step.info.id);
            after.push(mark);
        } else {
            before.push(mark);
        }
    }
    before.push(StripMark::Gate);
    before.extend(after);
    before
}

/// A draft of `text` on `repo`, the way the daemon builds one.
/// `committed` when the text is the Pipeline on the default branch.
fn draft(repo: &RepoName, text: &str, committed: bool) -> PipelineDraft {
    let outline = Outline::parse(text).expect("demo Pipelines parse");
    let problems = match load(text, &Builtins) {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    };
    let steps = outline
        .steps()
        .iter()
        .map(|step| {
            let plugin = plugin_of(&step.uses);
            let write = Builtins
                .plugin(&plugin)
                .is_some_and(|info| info.workspace == Workspace::Write);
            DraftStep {
                missing_secrets: if plugin == "jev" {
                    vec!["AI_GATEWAY_API_KEY".into()]
                } else {
                    Vec::new()
                },
                missing_plugin: None,
                merge: plugin == "merge",
                info: StepInfo {
                    id: step.id.clone(),
                    plugin,
                    needs: step.needs.clone(),
                    gated: outline.gate_role(&step.id) != GateRole::Advisory,
                    write,
                    condition: step.when.as_ref().map(flow_style),
                },
                uses: step.uses.clone(),
                with: step.with.clone(),
            }
        })
        .collect();
    let gate_terms = outline.gate().iter().map(gate_term).collect();
    PipelineDraft {
        repo: repo.clone(),
        base: DraftBase {
            branch: "main".into(),
            commit: "9f2c1e0".into(),
            blob: committed.then(|| "b10b".into()),
        },
        text: text.into(),
        edits: Vec::new(),
        steps,
        gate_terms,
        problems,
        positions: BTreeMap::new(),
        palette: palette(),
        published: None,
        conflicts: Vec::new(),
    }
}

fn gate_term(term: &Value) -> GateTerm {
    match Expr::parse_gate_term(term) {
        Ok(expr) => GateTerm::from(&expr),
        Err(_) => GateTerm::Other {
            text: flow_style(term),
        },
    }
}

/// The preset Library Steps, then the built-in Plugins.
fn palette() -> Vec<PaletteItem> {
    let item = |uses: String, summary: Option<String>| {
        let plugin = plugin_of(&uses);
        PaletteItem {
            write: plugin == "fix",
            merge: plugin == "merge",
            installed: true,
            plugin,
            uses,
            summary,
        }
    };
    let library = PRESETS.iter().map(|preset| {
        let summary = preset
            .text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# Shipped preset. "))
            .map(str::to_owned);
        item(format!("lib/{}", preset.name), summary)
    });
    let plugins = ["ci", "jev", "claude", "codex", "fix", "human", "merge"]
        .into_iter()
        .map(|name| item(name.to_owned(), None));
    library.chain(plugins).collect()
}

/// The Plugin a Step that `uses` this runs.
fn plugin_of(uses: &str) -> String {
    match uses.strip_prefix("lib/") {
        Some(name) => PRESETS
            .iter()
            .find(|preset| preset.name == name)
            .and_then(|preset| library_step_plugin(preset.text))
            .unwrap_or_else(|| name.to_owned()),
        None => uses.to_owned(),
    }
}

/// The built-in Plugins and the preset Library Steps.
struct Builtins;

impl Resolver for Builtins {
    fn library_step(&self, name: &str) -> Option<String> {
        let preset = PRESETS.iter().find(|preset| preset.name == name)?;
        Some(preset.text.to_owned())
    }

    fn plugin(&self, name: &str) -> Option<PluginInfo> {
        let workspace = match name {
            "fix" => Workspace::Write,
            "claude" | "codex" => Workspace::Read,
            "ci" | "jev" | "human" | "merge" => Workspace::None,
            _ => return None,
        };
        Some(PluginInfo {
            workspace,
            builtin: true,
        })
    }
}

fn pr(repo: &str, number: u64, title: &str, head_sha: &str) -> PullRequest {
    PullRequest {
        repo: repo_name(repo),
        number,
        title: title.into(),
        url: format!("https://github.com/{repo}/pull/{number}"),
        draft: false,
        head_sha: head_sha.into(),
        base: "main".into(),
        status: PrStatus::Ready,
        runs: Vec::new(),
        blocked: None,
        stack: None,
    }
}

/// `pr` stacked on PR `parent`.
fn stacked(mut pr: PullRequest, parent: u64, title: &str) -> PullRequest {
    pr.base = format!("stack/{parent}");
    pr.stack = Some(Box::new(StackPlace {
        parent: StackParent {
            number: parent,
            title: title.into(),
            url: format!("https://github.com/{}/pull/{parent}", pr.repo),
        },
        position: None,
        root_base: "main".into(),
    }));
    pr
}

fn unwatched(mut pr: PullRequest) -> PullRequest {
    pr.status = PrStatus::NotWatched;
    pr
}

fn finding(severity: Severity, message: &str, at: Option<(&str, u32)>) -> Finding {
    Finding {
        severity,
        message: message.into(),
        file: at.map(|(file, _)| file.into()),
        line: at.map(|(_, line)| line),
    }
}

fn repo_name(repo: &str) -> RepoName {
    let (owner, name) = repo.split_once('/').expect("owner/name");
    RepoName::new(owner, name)
}

fn pr_ref(repo: &str, number: u64) -> PrRef {
    PrRef {
        repo: repo_name(repo),
        number,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_demo_pipeline_loads_and_every_run_replays() {
        let fixture = Fixture::new();
        for (repo, text) in pipelines() {
            let draft = draft(&repo_name(repo), &text, true);
            assert!(draft.problems.is_empty(), "{repo}: {:?}", draft.problems);
        }
        for demo in &fixture.prs {
            for run in &demo.runs {
                let view = run.view();
                assert_eq!(view.seq, run.events.len() as u64, "Run {}", run.id);
                assert!(run.summary().strip.contains(&StripMark::Gate));
            }
        }
    }

    #[test]
    fn the_stress_cases_are_as_long_as_they_say() {
        assert_eq!(LONG_TITLE.chars().count(), 200);
        let fixture = Fixture::new();
        let long = fixture
            .prs
            .iter()
            .find(|demo| demo.pr.title == LONG_TITLE)
            .expect("the long PR");
        assert_eq!(long.runs.len(), 30);
        assert_eq!(long.runs[0].view().steps.len(), 30);
        let stack = fixture
            .prs
            .iter()
            .filter(|demo| (9001..=9020).contains(&demo.pr.number))
            .count();
        assert_eq!(stack, 20);
    }

    #[test]
    fn a_starter_fills_the_tour_draft() {
        let mut fixture = Fixture::new();
        let (updates, _) = fixture.answer(Command::ApplyStarter {
            repo: repo_name("jnsdls/dotfiles"),
            edits_seen: 0,
            starter: STARTERS[0].key.into(),
        });
        let [TopicUpdate::Pipeline { draft }] = &updates[..] else {
            panic!("one draft: {updates:?}");
        };
        assert!(!draft.steps.is_empty());
    }
}
