//! Onboarding a repo over the protocol: a repo without a Pipeline opens an
//! empty draft, a Starter fills it, the Steps that lack a Secret or a
//! Plugin say so, pasting the Secret clears that, and a Pipeline published
//! with a Secret unset holds its PRs behind the missing Secret's entry.

use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{STARTERS, load};
use slopwatch_daemon::drafts::Drafts;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::github::{GitHub, PIPELINE_PATH};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::pipeline::PipelineDraft;
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
use slopwatch_protocol::{
    Cause, ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, Inbox, InboxUpdate, PrRef,
    RepoName, ResponseBody, Scope, SecretValue, ServerFrame, Topic, TopicUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

/// The Secret the Jev presets need.
const JEV_SECRET: &str = "AI_GATEWAY_API_KEY";

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

struct Harness {
    github: Arc<FakeGitHub>,
    library: Arc<Library>,
    plugins: Arc<Plugins>,
    daemon: Arc<Daemon>,
    _data: tempfile::TempDir,
}

/// The daemon with Runs, drafts and the real built-in Plugins, over a
/// fake GitHub that has `jnsdls/app` with no Pipeline on main.
fn harness() -> Harness {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_slopwatchd");
    let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store.clone(),
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            clis: Default::default(),
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(exe, Arc::clone(&library)),
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();
    let plugins = Arc::clone(runs.plugins());
    let drafts = Drafts::new(
        store,
        Arc::clone(runs.plugins()) as _,
        Arc::clone(&library),
        Arc::clone(&runs) as _,
    )
    .with_needs(Arc::clone(&runs) as _);
    let daemon = Arc::new(
        Daemon::with_build_id("test", watching, Arc::clone(&library))
            .with_runs(runs)
            .with_drafts(Arc::new(drafts)),
    );
    Harness {
        github,
        library,
        plugins,
        daemon,
        _data: data,
    }
}

impl Harness {
    /// Swaps the Claude presets for stand-ins, so a test's Runs never start
    /// a real agent CLI, whichever Plugins this build has.
    fn stand_in_agents(&self) {
        self.library.save("claude-review", "uses: ci\n").unwrap();
        self.library.save("claude-fix", "uses: ci\n").unwrap();
    }
}

struct Client {
    client: InProcessClient,
    draft: Option<PipelineDraft>,
    inbox: Inbox,
}

impl Client {
    async fn connect(daemon: &Arc<Daemon>) -> Client {
        let mut client = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
        client
            .send(&ClientFrame::Hello(ClientHello::local()))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(ServerFrame::Hello(_))
        ));
        let mut client = Client {
            client,
            draft: None,
            inbox: Inbox::default(),
        };
        client.ok(Command::AddRepo { repo: repo() }).await;
        client
            .ok(Command::Subscribe {
                topic: Topic::Pipeline(repo()),
                since: None,
            })
            .await;
        client
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::Pipeline { draft } => self.draft = Some(*draft),
            TopicUpdate::Inbox { update, .. } => match update {
                InboxUpdate::Snapshot(snapshot) => self.inbox = snapshot,
                InboxUpdate::Delta(delta) => self.inbox.apply(delta),
            },
            _ => {}
        }
    }

    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.client.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => return response.result,
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
            frame = self.client.recv().await.unwrap();
        }
    }

    async fn ok(&mut self, command: Command) {
        match self.send(command).await {
            ResponseBody::Ok(_) => {}
            ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
        }
    }

    async fn refused(&mut self, command: Command) -> ErrorBody {
        match self.send(command).await {
            ResponseBody::Error(error) => error,
            ResponseBody::Ok(reply) => panic!("expected a refusal, got {reply:?}"),
        }
    }

    /// Applies frames until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.client.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn draft(&self) -> &PipelineDraft {
        self.draft.as_ref().expect("a draft arrived")
    }

    fn starter_command(&self, starter: &str) -> Command {
        Command::ApplyStarter {
            repo: repo(),
            edits_seen: self.draft().edits.len(),
            starter: starter.into(),
        }
    }

    async fn pick(&mut self, starter: &str) {
        self.ok(self.starter_command(starter)).await;
    }

    fn missing_secrets(&self, step: &str) -> Vec<String> {
        let step = self.draft().step(step).expect("the Step is in the draft");
        step.missing_secrets.clone()
    }
}

#[tokio::test]
async fn a_repo_without_a_pipeline_opens_an_empty_draft() {
    let harness = harness();
    let client = Client::connect(&harness.daemon).await;

    let draft = client.draft();
    assert_eq!(draft.base.blob, None, "main has no Pipeline file");
    assert!(draft.steps.is_empty());
    assert!(draft.edits.is_empty());
}

#[tokio::test]
async fn a_repo_with_a_pipeline_opens_a_draft_of_it() {
    let harness = harness();
    harness.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n",
    );
    let client = Client::connect(&harness.daemon).await;

    assert!(client.draft().base.blob.is_some(), "so onboarding skips");
}

#[tokio::test]
async fn each_starter_fills_the_draft_with_a_pipeline_that_loads_here() {
    // The shipped presets, with every built-in Plugin they use, `fix`
    // included, so no Starter has a missing Plugin.
    let harness = harness();
    let mut client = Client::connect(&harness.daemon).await;

    for starter in STARTERS {
        client.pick(starter.key).await;

        let draft = client.draft();
        assert!(
            draft.problems.is_empty(),
            "{}: {:#?}",
            starter.key,
            draft.problems
        );
        let missing: Vec<_> = draft
            .steps
            .iter()
            .filter_map(|step| step.missing_plugin.as_deref())
            .collect();
        assert!(missing.is_empty(), "{}: {missing:?}", starter.key);
        let ids: Vec<&str> = draft.steps.iter().map(|s| s.info.id.as_str()).collect();
        let expected = slopwatch_core::Outline::parse(starter.text).unwrap();
        let expected: Vec<&str> = expected.steps().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, expected, "{}", starter.key);
    }
}

#[tokio::test]
async fn a_starter_whose_plugin_isnt_installed_is_taken_and_the_step_says_so() {
    let harness = harness();
    harness
        .library
        .save("claude-review", "uses: not-installed-here\n")
        .unwrap();
    let mut client = Client::connect(&harness.daemon).await;

    client.pick("review-and-fix").await;

    let review = client.draft().step("claude-review").unwrap();
    assert_eq!(review.missing_plugin.as_deref(), Some("not-installed-here"));
    assert_eq!(client.draft().step("ci").unwrap().missing_plugin, None);
    // Dropping such a Step from the palette is still refused, as before.
    let (_, edits) = slopwatch_core::Outline::parse(&client.draft().text)
        .unwrap()
        .add_step("lib/claude-review", &[]);
    let refused = client
        .refused(Command::EditPipeline {
            repo: repo(),
            edits_seen: client.draft().edits.len(),
            edits,
            positions: Default::default(),
        })
        .await;
    assert!(refused.message.contains("isn't installed"), "{refused:?}");
}

#[tokio::test]
async fn an_unknown_or_stale_starter_pick_is_refused() {
    let harness = harness();
    let mut client = Client::connect(&harness.daemon).await;

    let unknown = client.refused(client.starter_command("everything")).await;
    assert_eq!(unknown.code, ErrorCode::NotFound);
    assert!(unknown.message.contains("everything"), "{unknown:?}");

    client.pick("just-ci").await;
    let stale = client
        .refused(Command::ApplyStarter {
            repo: repo(),
            edits_seen: 0,
            starter: "hands-off".into(),
        })
        .await;
    assert!(
        stale.message.contains("changed in the meantime"),
        "{stale:?}"
    );
}

#[tokio::test]
async fn pasting_a_missing_secret_clears_the_steps_badge() {
    let harness = harness();
    harness.stand_in_agents();
    let mut client = Client::connect(&harness.daemon).await;
    client.pick("ask-then-merge").await;

    assert_eq!(client.missing_secrets("desc-matches-diff"), [JEV_SECRET]);
    assert_eq!(client.missing_secrets("resolves-issue"), [JEV_SECRET]);
    assert!(client.missing_secrets("ci").is_empty());
    assert!(client.missing_secrets("ship-it").is_empty());

    client
        .ok(Command::SetSecret {
            secret: JEV_SECRET.into(),
            value: SecretValue::new("gateway-key-0123"),
        })
        .await;
    client
        .until("the draft to lose its badges", |c| {
            c.missing_secrets("desc-matches-diff").is_empty()
        })
        .await;

    assert!(client.missing_secrets("resolves-issue").is_empty());
}

#[tokio::test]
async fn a_pipeline_published_with_a_secret_unset_holds_its_prs_behind_the_secrets_entry() {
    let harness = harness();
    harness.stand_in_agents();
    harness.github.open_pr(&repo(), 1, "me", "Add the thing");
    harness.github.set_checks(
        &repo(),
        1,
        Checks {
            state: ChecksState::Success,
            runs: vec![Check {
                name: "test".into(),
                state: CheckState::Success,
                url: None,
                actions_job: None,
            }],
        },
    );
    let mut client = Client::connect(&harness.daemon).await;
    client
        .ok(Command::Subscribe {
            topic: Topic::Inbox,
            since: None,
        })
        .await;
    client.pick("review-and-fix").await;
    assert_eq!(client.missing_secrets("desc-matches-diff"), [JEV_SECRET]);

    client
        .ok(Command::PublishPipeline {
            repo: repo(),
            edits_seen: client.draft().edits.len(),
        })
        .await;
    client.ok(Command::MergePipeline { repo: repo() }).await;
    let landed = harness.github.file(&repo(), "main", PIPELINE_PATH).unwrap();
    assert!(load(&landed, harness.plugins.as_ref()).is_ok(), "{landed}");
    client
        .ok(Command::Watch {
            repo: repo(),
            number: 1,
        })
        .await;
    client.ok(Command::Refresh).await;

    let held = Scope::Cause {
        cause: Cause::MissingSecret {
            name: JEV_SECRET.into(),
        },
    };
    client
        .until("the missing Secret's entry to hold the PR", |c| {
            c.inbox.entries.iter().any(|entry| entry.scope == held)
        })
        .await;
    let entry = client
        .inbox
        .entries
        .iter()
        .find(|entry| entry.scope == held)
        .unwrap();
    assert_eq!(
        entry.prs,
        [PrRef {
            repo: repo(),
            number: 1
        }]
    );
}
