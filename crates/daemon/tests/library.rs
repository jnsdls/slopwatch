//! The Library: presets in a fresh one, editing over the protocol, and
//! Pipelines resolving its Steps live.

use std::path::Path;
use std::sync::Arc;

use slopwatch_core::{PluginInfo, Resolver, Workspace, load};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Watching};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, LibraryStep, Reply, ResponseBody,
    ServerFrame,
};
use tempfile::TempDir;

const PRESETS: [&str; 5] = [
    "claude-fix",
    "claude-review",
    "codex-review",
    "desc-matches-diff",
    "resolves-issue",
];

/// Resolves Library Steps through the daemon's Library and knows the
/// built-in Plugins, the way a Run's load does.
struct LibraryResolver<'a>(&'a Library);

impl Resolver for LibraryResolver<'_> {
    fn library_step(&self, name: &str) -> Option<String> {
        self.0.step(name)
    }

    fn plugin(&self, name: &str) -> Option<PluginInfo> {
        let workspace = match name {
            "jev" | "ci" | "human" | "merge" => Workspace::None,
            "claude" | "codex" => Workspace::Read,
            "fix" => Workspace::Write,
            _ => return None,
        };
        Some(PluginInfo {
            workspace,
            builtin: true,
        })
    }
}

fn fresh(home: &TempDir) -> Library {
    Library::open(home.path().join("steps")).unwrap()
}

fn names(library: &Library) -> Vec<String> {
    library
        .list()
        .unwrap()
        .into_iter()
        .map(|step| step.name)
        .collect()
}

fn daemon(library: Library) -> Arc<Daemon> {
    let github: Arc<dyn GitHub> = Arc::new(FakeGitHub::new("me"));
    let watching = Arc::new(Watching::new(Store::in_memory(), github).unwrap());
    Arc::new(Daemon::with_build_id("test", watching, Arc::new(library)))
}

async fn client(daemon: &Arc<Daemon>) -> InProcessClient {
    let mut client = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
    client
        .send(&ClientFrame::Hello(ClientHello::local()))
        .await
        .unwrap();
    assert!(matches!(
        client.recv().await.unwrap(),
        Some(ServerFrame::Hello(_))
    ));
    client
}

async fn send(client: &mut InProcessClient, command: Command) -> ResponseBody {
    match client.request(command).await.unwrap() {
        Some(ServerFrame::Response(response)) => response.result,
        other => panic!("expected a response, got {other:?}"),
    }
}

async fn list(client: &mut InProcessClient) -> Vec<LibraryStep> {
    match send(client, Command::ListLibrarySteps).await {
        ResponseBody::Ok(Reply::LibrarySteps { steps }) => steps,
        other => panic!("expected Library Steps, got {other:?}"),
    }
}

fn file(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("steps").join(format!("{name}.yml"))).unwrap()
}

#[test]
fn a_fresh_library_holds_the_five_presets_as_files() {
    let home = TempDir::new().unwrap();
    let library = fresh(&home);

    assert_eq!(names(&library), PRESETS);
    assert!(file(home.path(), "claude-fix").contains("agent: claude"));
    assert!(
        library
            .list()
            .unwrap()
            .iter()
            .all(|step| step.problem.is_none())
    );
}

#[test]
fn reopening_keeps_edits_and_deletions_of_presets() {
    let home = TempDir::new().unwrap();
    let library = fresh(&home);
    library
        .save("claude-review", "uses: claude\nwith: { model: sonnet }\n")
        .unwrap();
    library.delete("codex-review").unwrap();

    let reopened = fresh(&home);

    assert_eq!(
        names(&reopened),
        [
            "claude-fix",
            "claude-review",
            "desc-matches-diff",
            "resolves-issue"
        ]
    );
    assert_eq!(
        reopened.step("claude-review").unwrap(),
        "uses: claude\nwith: { model: sonnet }\n"
    );
}

#[test]
fn a_pipeline_applies_its_with_override_over_the_library_step() {
    let home = TempDir::new().unwrap();
    let library = fresh(&home);

    let pipeline = load(
        "version: 1\nsteps:\n  review: { uses: lib/claude-review, with: { model: sonnet } }\ngate: [review]\n",
        &LibraryResolver(&library),
    )
    .unwrap();

    let review = pipeline.step("review").unwrap();
    assert_eq!(review.config["model"], "sonnet");
    assert_eq!(review.config["auth"], "subscription");
    assert_eq!(review.config["fail_on"], "high");
}

#[test]
fn a_missing_library_step_makes_the_pipeline_invalid_naming_it() {
    let home = TempDir::new().unwrap();
    let library = fresh(&home);
    library.delete("resolves-issue").unwrap();

    let errors = load(
        "version: 1\nsteps:\n  issue: { uses: lib/resolves-issue }\ngate: [issue]\n",
        &LibraryResolver(&library),
    )
    .unwrap_err();

    let errors: Vec<_> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        errors,
        ["Step `issue` uses Library Step `resolves-issue`, which doesn't exist"]
    );
}

#[test]
fn a_name_that_isnt_one_file_in_the_library_resolves_to_nothing() {
    let home = TempDir::new().unwrap();
    let library = fresh(&home);
    std::fs::write(home.path().join("outside.yml"), "uses: ci\n").unwrap();

    assert_eq!(library.step("../outside"), None);
    assert!(library.save("../outside", "uses: ci\n").is_err());
}

#[tokio::test]
async fn the_editor_lists_the_presets() {
    let home = TempDir::new().unwrap();
    let mut client = client(&daemon(fresh(&home))).await;

    let steps = list(&mut client).await;

    let listed: Vec<_> = steps.iter().map(|step| step.name.as_str()).collect();
    assert_eq!(listed, PRESETS);
    assert_eq!(steps[0].text, file(home.path(), "claude-fix"));
}

#[tokio::test]
async fn editing_a_library_step_changes_every_pipeline_that_uses_it() {
    let home = TempDir::new().unwrap();
    let library = Library::open(home.path().join("steps")).unwrap();
    let daemon = daemon(fresh(&home));
    let mut client = client(&daemon).await;
    let review_and_fix = "version: 1\nsteps:\n  ci: { uses: ci }\n  review: { uses: lib/claude-review, needs: [ci] }\ngate: [ci, review]\n";
    let override_model = "version: 1\nsteps:\n  r: { uses: lib/claude-review, with: { model: sonnet } }\ngate: [r]\n";
    let before = |text| {
        let pipeline = load(text, &LibraryResolver(&library)).unwrap();
        let step = pipeline
            .steps()
            .find(|step| step.plugin == "claude")
            .unwrap();
        (step.config.clone(), step.reuse_key("abc", "1").config_hash)
    };
    let (first_before, first_hash) = before(review_and_fix);
    let (second_before, second_hash) = before(override_model);

    let reply = send(
        &mut client,
        Command::SaveLibraryStep {
            step: "claude-review".into(),
            text: "# tighter\nuses: claude\nwith:\n  model: opus\n  auth: subscription\n  fail_on: medium\n".into(),
        },
    )
    .await;
    assert_eq!(reply, ResponseBody::Ok(Reply::Done));

    let (first_after, first_hash_after) = before(review_and_fix);
    let (second_after, second_hash_after) = before(override_model);
    assert_eq!(first_before["fail_on"], "high");
    assert_eq!(first_after["fail_on"], "medium");
    assert_eq!(second_before["fail_on"], "high");
    assert_eq!(second_after["fail_on"], "medium");
    assert_eq!(second_after["model"], "sonnet", "the override still wins");
    assert_ne!(first_hash, first_hash_after);
    assert_ne!(second_hash, second_hash_after);
    let listed = list(&mut client).await;
    let saved = listed
        .iter()
        .find(|step| step.name == "claude-review")
        .unwrap();
    assert!(saved.text.starts_with("# tighter\n"), "comments are kept");
}

#[tokio::test]
async fn saving_a_step_that_wouldnt_load_is_refused_and_changes_nothing() {
    let home = TempDir::new().unwrap();
    let mut client = client(&daemon(fresh(&home))).await;
    let original = file(home.path(), "claude-review");

    let reply = send(
        &mut client,
        Command::SaveLibraryStep {
            step: "claude-review".into(),
            text: "uses: claude\nneeds: [ci]\n".into(),
        },
    )
    .await;

    let ResponseBody::Error(ErrorBody { code, message }) = reply else {
        panic!("expected an error, got {reply:?}");
    };
    assert_eq!(code, ErrorCode::Invalid);
    assert!(
        message.starts_with("Library Step `claude-review` is invalid:")
            && message.contains("unknown field `needs`"),
        "{message}"
    );
    assert_eq!(file(home.path(), "claude-review"), original);
}

#[tokio::test]
async fn a_new_step_can_be_saved_and_deleted() {
    let home = TempDir::new().unwrap();
    let mut client = client(&daemon(fresh(&home))).await;

    let saved = send(
        &mut client,
        Command::SaveLibraryStep {
            step: "docs-check".into(),
            text: "uses: jev\n".into(),
        },
    )
    .await;
    assert_eq!(saved, ResponseBody::Ok(Reply::Done));
    assert!(
        list(&mut client)
            .await
            .iter()
            .any(|step| step.name == "docs-check")
    );

    let deleted = send(
        &mut client,
        Command::DeleteLibraryStep {
            step: "docs-check".into(),
        },
    )
    .await;
    assert_eq!(deleted, ResponseBody::Ok(Reply::Done));
    let again = send(
        &mut client,
        Command::DeleteLibraryStep {
            step: "docs-check".into(),
        },
    )
    .await;
    assert!(
        matches!(
            &again,
            ResponseBody::Error(ErrorBody {
                code: ErrorCode::NotFound,
                ..
            })
        ),
        "{again:?}"
    );
}

#[tokio::test]
async fn a_hand_edited_step_that_wouldnt_load_lists_with_its_problem() {
    let home = TempDir::new().unwrap();
    let mut client = client(&daemon(fresh(&home))).await;
    std::fs::write(home.path().join("steps/broken.yml"), "uses: lib/other\n").unwrap();

    let steps = list(&mut client).await;

    let broken = steps.iter().find(|step| step.name == "broken").unwrap();
    assert_eq!(
        broken.problem.as_deref(),
        Some("it uses `lib/other`, but a Library Step must use a Plugin")
    );
}
