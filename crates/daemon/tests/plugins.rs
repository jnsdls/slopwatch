//! Third-party Plugins and their Approval (ADR 0012): found in the Plugins
//! folder, described locked down, held back until approved, kept approved
//! through a rebuild that asks for nothing more, and paused by one that
//! asks for more. The daemon runs over the in-process transport against a
//! fake GitHub, with shell-script Plugins in a temporary folder.

mod support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{EndReason, Verdict, Workspace};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::EffectKind;
use slopwatch_protocol::{
    Cause, Cli, CliSettings, CliStatus, ClientFrame, ClientHello, Command, ErrorCode, Grant, Inbox,
    InboxUpdate, Login, PluginListing, PluginSettings, PrRef, Reply, RepoName, ResponseBody, RunId,
    RunView, Scope, ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(30);

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn pr(number: u64) -> PrRef {
    PrRef {
        repo: repo(),
        number,
    }
}

/// One Step on the `lint` Plugin, which the Gate reads. `note` changes
/// the text, so writing it again counts as a Pipeline change.
fn pipeline(note: &str) -> String {
    format!("# {note}\nversion: 1\nsteps:\n  lint: {{ uses: lint }}\ngate: [lint]\n")
}

/// What `lint` asks for at first.
fn asks() -> Grant {
    Grant {
        workspace: Workspace::Read,
        effects: vec![EffectKind::Comment],
        secrets: vec![],
    }
}

struct Harness {
    /// First, so the Steps die before their folder goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    folder: PathBuf,
    _data: tempfile::TempDir,
    _plugins: tempfile::TempDir,
}

impl Harness {
    /// My labelled PRs `prs` in `jnsdls/app`, with `pipeline("one")` on
    /// main, and a `lint` Plugin in the folder asking for [`asks`].
    async fn new(prs: &[u64]) -> Self {
        let plugins = tempfile::tempdir().unwrap();
        let folder = plugins.path().join("plugins");
        std::fs::create_dir(&folder).unwrap();
        build_lint(&folder, &asks(), 1);
        Self::with_folder(prs, plugins, folder).await
    }

    async fn with_folder(prs: &[u64], plugins: tempfile::TempDir, folder: PathBuf) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        for &number in prs {
            github.open_pr(&repo(), number, "me", &format!("PR {number}"));
            github.label_on_github(&repo(), number, true);
        }
        github.set_pipeline(&repo(), "main", &pipeline("one"));
        let data = tempfile::tempdir().unwrap();
        let store = Store::open(&data.path().join("state.db")).unwrap();
        let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
        let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
        let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
        let found = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library))
            .with_describe_timeout(Duration::from_secs(3))
            .in_folder(&folder, "/usr/bin:/bin")
            .await;
        let runs = Runs::start(
            store,
            dyn_github,
            Arc::clone(&watching),
            RunsConfig {
                clis: Default::default(),
                data_dir: data.path().to_owned(),
                plugins: found,
                login_path: None,
                retention: Retention::default(),
                keychain: Arc::new(MemoryKeychain::default()),
            },
        )
        .unwrap();
        let daemon = Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs));
        Harness {
            _reaper: support::Reaper::new(plugins.path()),
            github,
            daemon,
            folder,
            _data: data,
            _plugins: plugins,
        }
    }

    /// How many sessions `lint` has run.
    fn lint_runs(&self) -> usize {
        std::fs::read_to_string(self.folder.parent().unwrap().join("lint.runs"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// Commits a new Pipeline text on main, which starts a same-SHA Run
    /// on every ended PR.
    async fn change_pipeline(&self, client: &mut Client, note: &str) {
        self.github.set_pipeline(&repo(), "main", &pipeline(note));
        client.ok(Command::Refresh).await;
    }
}

/// Writes the `lint` Plugin into `folder` the way a build would: a new
/// file renamed into place. `describe` prints a manifest asking for
/// `asks`. `run` notes the session next to the folder and passes. `build`
/// lands in a comment, so a rebuild changes the file but not the manifest.
fn build_lint(folder: &Path, asks: &Grant, build: u32) {
    let manifest = serde_json::json!({
        "id": "lint", "version": "1.0.0", "dialect": 1,
        "workspace": asks.workspace, "effects": asks.effects, "secrets": asks.secrets,
    });
    let runs = folder.parent().unwrap().join("lint.runs");
    let script = format!(
        "#!/bin/sh\n# build {build}\ncase \"$1\" in\n  describe) echo '{manifest}' ;;\n  \
         run) read -r start; echo run >> '{}'; \
         printf '{{\"type\":\"outcome\",\"verdict\":\"pass\"}}\\n' ;;\nesac\n",
        runs.display()
    );
    write_executable(folder, "lint", &script);
}

fn write_executable(folder: &Path, name: &str, script: &str) {
    let next = folder.join(format!(".{name}.next"));
    std::fs::write(&next, script).unwrap();
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&next, folder.join(name)).unwrap();
}

#[tokio::test]
async fn a_new_plugin_holds_its_prs_on_one_entry_until_approved() {
    let harness = Harness::new(&[1, 2]).await;
    let mut client = Client::connect(&harness.daemon).await;

    let listed = client.plugin("lint").await;
    assert_eq!(listed.asks, Some(asks()));
    assert_eq!(listed.approved, None);
    assert!(listed.needs_approval());
    assert!(listed.path.unwrap().ends_with("/plugins/lint"));

    client.ok(Command::AddRepo { repo: repo() }).await;
    let firsts = [client.history(1)[0], client.history(2)[0]];
    for run in firsts {
        client.subscribe(run).await;
    }
    client
        .until("both Runs to end", |c| {
            firsts.iter().all(|run| c.end(*run).is_some())
        })
        .await;
    for run in firsts {
        assert_eq!(client.end(run), Some(EndReason::NotShippable));
        assert_eq!(
            client.reason(run, "lint").as_deref(),
            Some("error(plugin unapproved): Plugin `lint` hasn't been approved")
        );
    }
    assert_eq!(harness.lint_runs(), 0, "nothing ran before the Approval");
    // One shared entry holds both PRs, and no Run or PR entry repeats it.
    assert_eq!(client.inbox.count(), 1, "{:#?}", client.inbox);
    let entry = &client.inbox.entries[0];
    assert_eq!(
        entry.scope,
        Scope::Cause {
            cause: Cause::UnapprovedPlugin {
                plugin: "lint".into()
            }
        }
    );
    assert_eq!(entry.prs, vec![pr(1), pr(2)]);
    assert_eq!(entry.title, "Plugin `lint` needs approval");

    client
        .ok(Command::ApprovePlugin {
            plugin: "lint".into(),
            grant: asks(),
        })
        .await;
    client
        .until("a same-SHA Run on each PR", |c| {
            c.history(1).len() == 2 && c.history(2).len() == 2
        })
        .await;
    let seconds = [client.history(1)[0], client.history(2)[0]];
    for run in seconds {
        client.subscribe(run).await;
    }
    client
        .until("both Runs to ship", |c| {
            seconds
                .iter()
                .all(|run| c.end(*run) == Some(EndReason::Shippable))
        })
        .await;
    assert_eq!(client.inbox.count(), 0);
    assert_eq!(harness.lint_runs(), 2);

    let listed = client.plugin("lint").await;
    assert_eq!(listed.approved, Some(asks()));
    assert!(listed.approved_at.is_some());
    assert!(!listed.needs_approval());
}

#[tokio::test]
async fn a_rebuild_with_the_same_manifest_stays_approved_and_reruns_its_steps() {
    let harness = Harness::new(&[1]).await;
    let mut client = Client::connect(&harness.daemon).await;
    client.approve(asks()).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let first = client.next_run(1, 1).await;
    assert_eq!(client.end(first), Some(EndReason::Shippable));
    assert_eq!(harness.lint_runs(), 1);

    // Same build: a new Pipeline text reuses the Outcome.
    harness.change_pipeline(&mut client, "two").await;
    let second = client.next_run(1, 2).await;
    assert_eq!(client.reused(second, "lint"), Some(first));
    assert_eq!(harness.lint_runs(), 1);

    // A rebuild asking for the same: still approved, and it runs again.
    build_lint(&harness.folder, &asks(), 2);
    harness.change_pipeline(&mut client, "three").await;
    let third = client.next_run(1, 3).await;
    assert_eq!(client.end(third), Some(EndReason::Shippable));
    assert_eq!(client.reused(third, "lint"), None);
    assert_eq!(harness.lint_runs(), 2);
    assert_eq!(client.inbox.count(), 0);
}

#[tokio::test]
async fn a_manifest_that_asks_for_more_pauses_the_plugin_until_approved_again() {
    let harness = Harness::new(&[1]).await;
    let mut client = Client::connect(&harness.daemon).await;
    client.approve(asks()).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let first = client.next_run(1, 1).await;
    assert_eq!(client.end(first), Some(EndReason::Shippable));

    let more = Grant {
        effects: vec![EffectKind::Comment, EffectKind::Merge],
        secrets: vec!["LINT_KEY".into()],
        ..asks()
    };
    build_lint(&harness.folder, &more, 2);
    harness.change_pipeline(&mut client, "two").await;
    let second = client.next_run(1, 2).await;
    assert_eq!(client.end(second), Some(EndReason::NotShippable));
    assert_eq!(
        client.reason(second, "lint").as_deref(),
        Some(
            "error(plugin unapproved): Plugin `lint` now asks for the `merge` Effect, Secret \
             `LINT_KEY`, which its Approval doesn't cover"
        )
    );
    assert_eq!(harness.lint_runs(), 1);
    assert_eq!(client.inbox.count(), 1);
    assert!(client.plugin("lint").await.needs_approval());

    // The old asks don't cover the new manifest.
    let (code, message) = client
        .refused(Command::ApprovePlugin {
            plugin: "lint".into(),
            grant: asks(),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("the `merge` Effect"), "{message}");

    // Approving it asks for the Secret next, which is a missing Secret's
    // entry now, not the Plugin's.
    client.approve(more).await;
    let third = client.next_run(1, 3).await;
    assert_eq!(
        client.reason(third, "lint").as_deref(),
        Some("error(secret missing): `LINT_KEY` isn't set")
    );
    assert_eq!(client.inbox.count(), 1);
    assert_eq!(
        client.inbox.entries[0].scope,
        Scope::Cause {
            cause: Cause::MissingSecret {
                name: "LINT_KEY".into()
            }
        }
    );
}

#[tokio::test]
async fn plugins_that_dont_load_are_listed_with_why_and_cant_be_approved() {
    let plugins = tempfile::tempdir().unwrap();
    let folder = plugins.path().join("plugins");
    std::fs::create_dir(&folder).unwrap();
    let ran = plugins.path().join("merge.ran");
    write_executable(
        &folder,
        "merge",
        &format!("#!/bin/sh\ntouch '{}'\n", ran.display()),
    );
    write_executable(&folder, "hangs", "#!/bin/sh\nexec sleep 30\n");
    let harness = Harness::with_folder(&[], plugins, folder).await;
    let mut client = Client::connect(&harness.daemon).await;

    let listed = client.plugins().await;
    let merges: Vec<&PluginListing> = listed.iter().filter(|p| p.name == "merge").collect();
    assert_eq!(merges.len(), 2, "{listed:#?}");
    // The built-in keeps the name, and the file never ran.
    assert!(merges[0].builtin && merges[0].problem.is_none());
    assert!(!merges[0].needs_approval());
    assert!(merges[1].problem.as_deref().unwrap().contains("reserved"));
    assert_eq!(merges[1].approved, None);
    assert!(!ran.exists());
    let hangs = listed.iter().find(|p| p.name == "hangs").unwrap();
    assert!(
        hangs.problem.as_deref().unwrap().contains("didn't finish"),
        "{hangs:?}"
    );
    assert!(!hangs.needs_approval());

    for (plugin, code) in [
        ("hangs", ErrorCode::Invalid),
        ("merge", ErrorCode::Invalid),
        ("nobody", ErrorCode::NotFound),
    ] {
        let (got, message) = client
            .refused(Command::ApprovePlugin {
                plugin: plugin.into(),
                grant: asks(),
            })
            .await;
        assert_eq!(got, code, "{plugin}: {message}");
    }
}

#[tokio::test]
async fn settings_take_absolute_paths_and_a_cap_and_show_in_the_list() {
    let harness = Harness::new(&[]).await;
    let mut client = Client::connect(&harness.daemon).await;

    let (code, _) = client
        .refused(Command::SetPluginSettings {
            plugin: "lint".into(),
            settings: PluginSettings {
                path: vec!["bin".into()],
                cap: None,
                config_dir: None,
            },
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    let (code, _) = client
        .refused(Command::SetPluginSettings {
            plugin: "nobody".into(),
            settings: PluginSettings::default(),
        })
        .await;
    assert_eq!(code, ErrorCode::NotFound);

    let settings = PluginSettings {
        path: vec!["/opt/lint/bin".into()],
        cap: Some(1),
        config_dir: None,
    };
    for plugin in ["lint", "ci"] {
        client
            .ok(Command::SetPluginSettings {
                plugin: plugin.into(),
                settings: settings.clone(),
            })
            .await;
        assert_eq!(client.plugin(plugin).await.settings, settings);
    }
}

#[tokio::test]
async fn the_agent_plugins_take_their_path_and_config_dir_from_the_cli_settings() {
    let harness = Harness::new(&[]).await;
    let mut client = Client::connect(&harness.daemon).await;

    for plugin in ["claude", "codex", "fix"] {
        let (code, message) = client
            .refused(Command::SetPluginSettings {
                plugin: plugin.into(),
                settings: PluginSettings {
                    config_dir: Some("/Users/me/.claude-work".into()),
                    ..PluginSettings::default()
                },
            })
            .await;
        assert_eq!(code, ErrorCode::Invalid, "{plugin}");
        assert!(message.contains("CLI's settings"), "{message}");
    }
    let cap = PluginSettings {
        cap: Some(1),
        ..PluginSettings::default()
    };
    client
        .ok(Command::SetPluginSettings {
            plugin: "claude".into(),
            settings: cap.clone(),
        })
        .await;
    assert_eq!(client.plugin("claude").await.settings, cap);
}

#[tokio::test]
async fn each_cli_lists_what_it_resolved_to_its_version_and_its_login() {
    let harness = Harness::new(&[]).await;
    let mut client = Client::connect(&harness.daemon).await;
    let dir = harness.folder.parent().unwrap();
    let fake = |name: &str, body: &str| {
        let file = dir.join(name);
        std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        file.display().to_string()
    };
    let claude = fake(
        "mclaude",
        r#"case "$1" in
--version) echo "2.1.0 (Claude Code)" ;;
auth) echo '{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "max"}' ;;
esac"#,
    );
    let codex = fake(
        "codex",
        r#"case "$1" in
--version) echo "codex-cli 0.159.0" ;;
login) echo "Not logged in" >&2; exit 1 ;;
esac"#,
    );
    let gh = fake("gh", r#"echo "gh version 2.90.0 (2026-04-16)""#);
    let missing = dir.join("no-git").display().to_string();

    for (cli, settings) in [
        (
            Cli::Gh,
            CliSettings {
                executable: Some("bin/gh".into()),
                ..CliSettings::default()
            },
        ),
        (
            Cli::Gh,
            CliSettings {
                path: vec!["/opt/bin".into()],
                ..CliSettings::default()
            },
        ),
    ] {
        let (code, _) = client
            .refused(Command::SetCliSettings { cli, settings })
            .await;
        assert_eq!(code, ErrorCode::Invalid);
    }
    for (cli, executable) in [
        (Cli::Claude, &claude),
        (Cli::Codex, &codex),
        (Cli::Gh, &gh),
        (Cli::Git, &missing),
    ] {
        client
            .ok(Command::SetCliSettings {
                cli,
                settings: CliSettings {
                    executable: Some(executable.clone()),
                    ..CliSettings::default()
                },
            })
            .await;
    }

    let Reply::Clis { clis } = client.ok(Command::ListClis).await else {
        panic!("expected Clis");
    };
    let listed: Vec<Cli> = clis.iter().map(|listing| listing.cli).collect();
    assert_eq!(listed, Cli::ALL);
    let status = |cli: Cli| &clis.iter().find(|l| l.cli == cli).unwrap().status;
    assert_eq!(
        status(Cli::Claude),
        &CliStatus {
            resolved: Some(claude.clone()),
            version: Some("2.1.0 (Claude Code)".into()),
            login: Some(Login {
                logged_in: true,
                detail: Some("claude.ai (max)".into()),
            }),
            problem: None,
        }
    );
    assert_eq!(
        status(Cli::Codex).login,
        Some(Login {
            logged_in: false,
            detail: None,
        })
    );
    assert_eq!(
        status(Cli::Gh).version.as_deref(),
        Some("gh version 2.90.0 (2026-04-16)")
    );
    assert_eq!(status(Cli::Gh).login, None);
    assert_eq!(
        status(Cli::Git).problem,
        Some(format!("`{missing}` doesn't exist"))
    );
    assert_eq!(
        clis[0].settings.executable.as_deref(),
        Some(claude.as_str())
    );
}

/// A client that keeps its copy of `watched_prs`, the Inbox and its Runs.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    inbox: Inbox,
    runs: HashMap<RunId, RunView>,
}

impl Client {
    async fn connect(daemon: &Arc<Daemon>) -> Self {
        let mut connection = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
        connection
            .send(&ClientFrame::Hello(ClientHello::local()))
            .await
            .unwrap();
        let hello = connection.recv().await.unwrap();
        assert!(matches!(hello, Some(ServerFrame::Hello(_))), "{hello:?}");
        let mut client = Self {
            connection,
            prs: WatchedPrs::default(),
            inbox: Inbox::default(),
            runs: HashMap::new(),
        };
        for topic in [Topic::WatchedPrs, Topic::Inbox] {
            client.ok(Command::Subscribe { topic, since: None }).await;
        }
        client
    }

    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.connection.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => return response.result,
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
            frame = self.connection.recv().await.unwrap();
        }
    }

    async fn ok(&mut self, command: Command) -> Reply {
        match self.send(command).await {
            ResponseBody::Ok(reply) => reply,
            ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
        }
    }

    async fn refused(&mut self, command: Command) -> (ErrorCode, String) {
        match self.send(command).await {
            ResponseBody::Error(error) => (error.code, error.message),
            ResponseBody::Ok(reply) => panic!("expected an error, got {reply:?}"),
        }
    }

    async fn plugins(&mut self) -> Vec<PluginListing> {
        match self.ok(Command::ListPlugins).await {
            Reply::Plugins { plugins } => plugins,
            other => panic!("expected Plugins, got {other:?}"),
        }
    }

    async fn plugin(&mut self, name: &str) -> PluginListing {
        self.plugins()
            .await
            .into_iter()
            .find(|plugin| plugin.name == name)
            .unwrap_or_else(|| panic!("no Plugin `{name}` listed"))
    }

    async fn approve(&mut self, grant: Grant) {
        self.ok(Command::ApprovePlugin {
            plugin: "lint".into(),
            grant,
        })
        .await;
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            TopicUpdate::Inbox { update, .. } => match update {
                InboxUpdate::Snapshot(snapshot) => self.inbox = snapshot,
                InboxUpdate::Delta(delta) => self.inbox.apply(delta),
            },
            TopicUpdate::Run { id, seq, event, .. } => {
                self.runs.entry(id).or_default().apply(seq, event);
            }
            TopicUpdate::StepLog { .. }
            | TopicUpdate::Notifications { .. }
            | TopicUpdate::Pipeline { .. } => {}
        }
    }

    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.connection.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {what}: {:#?} {:#?}",
                        self.inbox, self.runs
                    )
                })
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn subscribe(&mut self, run: RunId) {
        self.ok(Command::Subscribe {
            topic: Topic::Run(run),
            since: None,
        })
        .await;
    }

    /// Waits for PR `number`'s `count`th Run, follows it until it ends,
    /// and returns it.
    async fn next_run(&mut self, number: u64, count: usize) -> RunId {
        self.until(&format!("Run {count} on #{number}"), |c| {
            c.history(number).len() == count
        })
        .await;
        let run = self.history(number)[0];
        self.subscribe(run).await;
        self.until("the Run to end", |c| c.end(run).is_some()).await;
        run
    }

    /// PR `number`'s Runs, newest first.
    fn history(&self, number: u64) -> Vec<RunId> {
        self.prs
            .pr(&repo(), number)
            .map(|pr| pr.runs.iter().map(|run| run.id).collect())
            .unwrap_or_default()
    }

    fn end(&self, run: RunId) -> Option<EndReason> {
        self.runs.get(&run).and_then(|view| view.end)
    }

    fn step(&self, run: RunId, step: &str) -> Option<&StepStatus> {
        let view = self.runs.get(&run)?;
        view.steps
            .iter()
            .find(|view| view.info.id == step)
            .map(|view| &view.status)
    }

    fn reason(&self, run: RunId, step: &str) -> Option<String> {
        match self.step(run, step)? {
            StepStatus::Settled { reason, .. } => reason.clone(),
            _ => None,
        }
    }

    /// The Run a settled pass was reused from, if it was.
    fn reused(&self, run: RunId, step: &str) -> Option<RunId> {
        match self.step(run, step)? {
            StepStatus::Settled {
                verdict: Verdict::Pass,
                reused_from,
                ..
            } => *reused_from,
            other => panic!("`{step}` didn't pass: {other:?}"),
        }
    }
}
