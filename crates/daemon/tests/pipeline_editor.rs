//! The Pipeline editor over the protocol: the draft a repo's topic carries,
//! edits collecting in it, gestures refused with core's reason, and node
//! positions the daemon keeps out of the repo.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use slopwatch_core::{Edit, Outline, PluginInfo, Resolver, Target, Workspace};
use slopwatch_daemon::drafts::{BaseFile, Drafts, PipelineSource};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Watching};
use slopwatch_protocol::pipeline::{DraftBase, NodePosition, PipelineDraft};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, GateTerm, RepoName, ResponseBody,
    ServerFrame, Topic, TopicUpdate,
};
use tempfile::TempDir;

const BASE: &str = "version: 1\n# Checks first.\nsteps:\n  ci: { uses: ci }\n  review: { uses: lib/claude-review, needs: [ci] }\ngate: [ci, review]\n";

fn repo() -> RepoName {
    RepoName::new("o", "r")
}

/// The built-in Plugins, as their manifests will declare them, and the
/// Library read live.
struct Builtins(Arc<Library>);

impl Resolver for Builtins {
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

/// The default branch, as a test sets it.
struct Branch(Mutex<BaseFile>);

impl Branch {
    fn new(text: Option<&str>) -> Arc<Self> {
        Arc::new(Branch(Mutex::new(BaseFile {
            base: DraftBase {
                branch: "main".into(),
                commit: "c1".into(),
                blob: text.map(|_| "b1".into()),
            },
            text: text.map(str::to_owned),
        })))
    }

    fn commit(&self, sha: &str, text: &str) {
        let mut file = self.0.lock().unwrap();
        file.base.commit = sha.into();
        file.base.blob = Some(format!("blob-{sha}"));
        file.text = Some(text.into());
    }
}

#[async_trait]
impl PipelineSource for Branch {
    async fn default_pipeline(&self, _: &RepoName) -> Result<BaseFile, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

/// A daemon over a database in `home`, so a second one sees what the
/// first kept, as after a restart.
fn daemon(home: &TempDir, branch: &Arc<Branch>) -> Arc<Daemon> {
    let store = Store::open(&home.path().join("state.db")).unwrap();
    store.add_repo(&repo()).unwrap();
    let library = Arc::new(Library::open(home.path().join("steps")).unwrap());
    let github: Arc<dyn GitHub> = Arc::new(FakeGitHub::new("me"));
    let watching = Arc::new(Watching::new(store.clone(), github).unwrap());
    let drafts = Drafts::new(
        store,
        Arc::new(Builtins(Arc::clone(&library))),
        Arc::clone(&library),
        Arc::clone(branch) as Arc<dyn PipelineSource>,
    );
    Arc::new(Daemon::with_build_id("test", watching, library).with_drafts(Arc::new(drafts)))
}

struct Client {
    client: InProcessClient,
    /// The last draft the topic carried.
    draft: Option<PipelineDraft>,
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
        Client {
            client,
            draft: None,
        }
    }

    /// Sends `command` and returns its result, taking the topic frames
    /// that come before it.
    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.client.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => return response.result,
                Some(ServerFrame::Topic(TopicUpdate::Pipeline { draft })) => {
                    self.draft = Some(*draft);
                }
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

    async fn open(&mut self) -> &PipelineDraft {
        self.ok(Command::Subscribe {
            topic: Topic::Pipeline(repo()),
            since: None,
        })
        .await;
        self.draft()
    }

    fn draft(&self) -> &PipelineDraft {
        self.draft.as_ref().expect("a draft arrived")
    }

    fn outline(&self) -> Outline {
        Outline::parse(&self.draft().text).unwrap()
    }

    /// Edits the draft as this client last saw it.
    async fn edit(&mut self, edits: Vec<Edit>) -> ResponseBody {
        self.send(Command::EditPipeline {
            repo: repo(),
            edits_seen: self.draft().edits.len(),
            edits,
            positions: BTreeMap::new(),
        })
        .await
    }

    async fn edit_ok(&mut self, edits: Vec<Edit>) {
        match self.edit(edits).await {
            ResponseBody::Ok(_) => {}
            ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
        }
    }
}

#[tokio::test]
async fn a_repos_draft_starts_from_its_default_branch_with_the_palette() {
    let home = TempDir::new().unwrap();
    let branch = Branch::new(Some(BASE));
    let mut client = Client::connect(&daemon(&home, &branch)).await;

    let draft = client.open().await.clone();

    assert_eq!(draft.text, BASE);
    assert!(draft.edits.is_empty());
    assert_eq!(draft.base.blob.as_deref(), Some("b1"));
    assert!(draft.problems.is_empty(), "{:?}", draft.problems);
    let review = draft.step("review").unwrap();
    assert_eq!(review.info.plugin, "claude");
    assert!(review.info.gated);
    assert_eq!(review.uses, "lib/claude-review");
    let palette: Vec<(&str, bool)> = draft
        .palette
        .iter()
        .map(|item| (item.uses.as_str(), item.after_gate()))
        .collect();
    assert_eq!(
        palette,
        [
            ("lib/claude-fix", true),
            ("lib/claude-review", false),
            ("lib/codex-review", false),
            ("lib/desc-matches-diff", false),
            ("lib/resolves-issue", false),
            ("jev", false),
            ("ci", false),
            ("claude", false),
            ("codex", false),
            ("fix", true),
            ("human", false),
            ("merge", true),
        ]
    );
    assert_eq!(
        draft.palette[1].summary.as_deref(),
        Some(
            "Runs `claude -p` as a reviewer in a checkout of the PR's head, with read-only tools."
        )
    );
}

#[tokio::test]
async fn a_repo_without_a_pipeline_starts_an_empty_draft() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(None))).await;

    let draft = client.open().await;

    assert_eq!(draft.text, "version: 1\nsteps: {}\ngate: []\n");
    assert_eq!(draft.base.blob, None);
    assert_eq!(draft.problems, ["the Gate references no Steps"]);
}

#[tokio::test]
async fn dragging_a_library_step_onto_the_canvas_adds_it_to_the_draft() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;

    let (id, edits) = client.outline().add_step("lib/desc-matches-diff", &[]);
    client.edit_ok(edits.clone()).await;

    let draft = client.draft();
    assert_eq!(id, "desc-matches-diff");
    assert_eq!(draft.edits, edits);
    assert!(
        draft
            .text
            .contains("  desc-matches-diff: { uses: lib/desc-matches-diff }\n")
    );
    let step = draft.step("desc-matches-diff").unwrap();
    assert_eq!(step.info.plugin, "jev");
    assert!(!step.info.gated);
}

#[tokio::test]
async fn wiring_ports_adds_needs_edges_and_gate_terms() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;
    let (_, edits) = client.outline().add_step("human", &[]);
    client.edit_ok(edits).await;

    let edits = client.outline().connect("review", Target::Step("human"));
    client.edit_ok(edits).await;
    let edits = client.outline().connect("human", Target::GateAnyOf);
    client.edit_ok(edits).await;

    let draft = client.draft();
    assert_eq!(draft.step("human").unwrap().info.needs, ["review"]);
    assert!(draft.step("human").unwrap().info.gated);
    assert_eq!(
        draft.gate_terms.last().unwrap(),
        &GateTerm::AnyOf {
            terms: vec![GateTerm::Step {
                id: "human".into(),
                accepts_skipped: false
            }]
        }
    );
}

#[tokio::test]
async fn a_cycle_or_a_gate_term_on_a_write_step_is_refused_with_the_reason() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;
    let (fix, edits) = client.outline().add_step("lib/claude-fix", &["gate"]);
    client.edit_ok(edits).await;
    let before = client.draft().clone();

    let cycle = client.outline().connect("review", Target::Step("ci"));
    let refused = client.edit(cycle).await;
    assert_eq!(
        refused,
        ResponseBody::Error(ErrorBody {
            code: ErrorCode::Invalid,
            message: "the Pipeline has a cycle: ci -> review -> ci".into(),
        })
    );

    let (autofix, edits) = client.outline().add_step("fix", &[]);
    client.edit_ok(edits).await;
    let gated = client.outline().connect(&autofix, Target::Gate);
    let ResponseBody::Error(error) = client.edit(gated).await else {
        panic!("expected a refusal");
    };
    assert_eq!(
        error.message,
        "the Gate references `fix`, which declares `workspace: write`"
    );

    let after_gate = client.outline().connect(&fix, Target::Gate);
    let ResponseBody::Error(error) = client.edit(after_gate).await else {
        panic!("expected a refusal");
    };
    assert_eq!(
        error.message,
        "the Pipeline has a cycle: claude-fix -> gate -> claude-fix"
    );
    assert_eq!(
        client.draft().edits.len(),
        before.edits.len() + 1,
        "refusals change nothing"
    );
}

#[tokio::test]
async fn inspector_edits_change_the_draft() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;
    let outline = client.outline();
    let with = serde_json::Map::from_iter([("model".to_owned(), serde_json::json!("sonnet"))]);
    let mut edits = outline.set_with("review", &with);
    edits.extend(outline.set_condition("review", Some(&serde_json::json!({ "files": "src/**" }))));
    edits.extend(outline.set_gate_role("review", slopwatch_core::GateRole::RequiredOrSkipped));

    client.edit_ok(edits).await;

    let review = client.draft().step("review").unwrap();
    assert_eq!(review.with, with);
    assert_eq!(review.info.condition.as_deref(), Some("{ files: src/** }"));
    assert_eq!(
        client.draft().gate_terms[1],
        GateTerm::Step {
            id: "review".into(),
            accepts_skipped: true
        }
    );
}

#[tokio::test]
async fn positions_survive_a_restart_stay_out_of_the_file_and_tidy_resets_them() {
    let home = TempDir::new().unwrap();
    let branch = Branch::new(Some(BASE));
    let mut client = Client::connect(&daemon(&home, &branch)).await;
    client.open().await;
    client
        .ok(Command::MovePipelineNode {
            repo: repo(),
            node: "review".into(),
            position: NodePosition { x: 320, y: 140 },
        })
        .await;
    client
        .ok(Command::MovePipelineNode {
            repo: repo(),
            node: "gate".into(),
            position: NodePosition { x: 600, y: 40 },
        })
        .await;
    let unknown = client
        .send(Command::MovePipelineNode {
            repo: repo(),
            node: "nope".into(),
            position: NodePosition { x: 0, y: 0 },
        })
        .await;
    assert!(matches!(
        unknown,
        ResponseBody::Error(ErrorBody {
            code: ErrorCode::NotFound,
            ..
        })
    ));
    drop(client);

    let mut client = Client::connect(&daemon(&home, &branch)).await;
    let draft = client.open().await.clone();
    assert_eq!(draft.positions["review"], NodePosition { x: 320, y: 140 });
    assert_eq!(draft.positions["gate"], NodePosition { x: 600, y: 40 });
    assert_eq!(
        draft.text, BASE,
        "positions never go into the Pipeline file"
    );
    assert!(draft.edits.is_empty());

    client.ok(Command::TidyPipeline { repo: repo() }).await;
    assert!(client.draft().positions.is_empty());
}

#[tokio::test]
async fn removing_a_step_drops_its_position() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;
    client
        .ok(Command::MovePipelineNode {
            repo: repo(),
            node: "review".into(),
            position: NodePosition { x: 10, y: 10 },
        })
        .await;

    let edits = client.outline().remove_step("review");
    client.edit_ok(edits).await;

    assert!(client.draft().step("review").is_none());
    assert!(client.draft().positions.is_empty());
}

#[tokio::test]
async fn a_draft_follows_its_branch_until_it_has_edits() {
    let home = TempDir::new().unwrap();
    let branch = Branch::new(Some(BASE));
    let daemon = daemon(&home, &branch);
    Client::connect(&daemon).await.open().await;

    let moved = BASE.replace("gate: [ci, review]", "gate: [ci]");
    branch.commit("c2", &moved);
    let mut client = Client::connect(&daemon).await;
    assert_eq!(client.open().await.text, moved);
    assert_eq!(client.draft().base.commit, "c2");

    let (_, edits) = client.outline().add_step("ci", &[]);
    client.edit_ok(edits).await;
    branch.commit("c3", BASE);
    let mut client = Client::connect(&daemon).await;
    let draft = client.open().await;
    assert_eq!(draft.base.commit, "c2", "a draft with edits keeps its base");
    assert!(draft.text.contains("ci-2: { uses: ci }"));
}

#[tokio::test]
async fn an_unknown_repo_has_no_draft() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(None))).await;

    let result = client
        .send(Command::Subscribe {
            topic: Topic::Pipeline(RepoName::new("o", "elsewhere")),
            since: None,
        })
        .await;

    assert_eq!(
        result,
        ResponseBody::Error(ErrorBody {
            code: ErrorCode::NotFound,
            message: "Not found: repo o/elsewhere".into(),
        })
    );
}

#[tokio::test]
async fn a_step_dropped_on_the_canvas_lands_where_it_was_dropped_in_one_change() {
    let home = TempDir::new().unwrap();
    let mut client = Client::connect(&daemon(&home, &Branch::new(Some(BASE)))).await;
    client.open().await;
    let (id, edits) = client.outline().add_step("human", &[]);
    let at = NodePosition { x: 480, y: 220 };

    client
        .ok(Command::EditPipeline {
            repo: repo(),
            edits_seen: 0,
            edits,
            positions: BTreeMap::from([(id.clone(), at)]),
        })
        .await;

    assert!(client.draft().step(&id).is_some());
    assert_eq!(client.draft().positions[&id], at);

    // A refused edit places nothing.
    let (other, edits) = client.outline().add_step("nothing-installed", &[]);
    let refused = client
        .send(Command::EditPipeline {
            repo: repo(),
            edits_seen: 1,
            edits,
            positions: BTreeMap::from([(other.clone(), at)]),
        })
        .await;
    assert_eq!(
        refused,
        ResponseBody::Error(ErrorBody {
            code: ErrorCode::Invalid,
            message:
                "Step `nothing-installed` uses Plugin `nothing-installed`, which isn't installed"
                    .into(),
        })
    );
    assert!(!client.draft().positions.contains_key(&other));
}

#[tokio::test]
async fn edits_made_on_an_older_draft_are_refused() {
    let home = TempDir::new().unwrap();
    let daemon = daemon(&home, &Branch::new(Some(BASE)));
    let mut first = Client::connect(&daemon).await;
    let mut second = Client::connect(&daemon).await;
    first.open().await;
    second.open().await;
    // Both see the Gate as [ci, review]; the first takes `ci` out.
    let edits = first
        .outline()
        .set_gate_role("ci", slopwatch_core::GateRole::Advisory);
    first.edit_ok(edits).await;

    // The second still thinks term 1 is `review`.
    let stale = second
        .outline()
        .set_gate_role("review", slopwatch_core::GateRole::Advisory);
    let refused = second.edit(stale).await;

    assert_eq!(
        refused,
        ResponseBody::Error(ErrorBody {
            code: ErrorCode::Invalid,
            message: "The draft changed in the meantime. Try again on the draft as it is now."
                .into(),
        })
    );
}
