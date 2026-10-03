//! Draft Pipelines: what the developer edits on the canvas before
//! publishing (ADR 0007). A draft is the Pipeline file on the repo's default
//! branch plus the edits since, and the daemon keeps it, so it outlives the
//! GUI. Beside it sits the sidecar of node positions, which never goes into
//! the repo.
//!
//! Every edit goes through core's [`try_edits`], so a gesture that would
//! make the Pipeline invalid is refused with the loader's own reason.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use slopwatch_core::{
    BUILTIN_PLUGINS, EMPTY_PIPELINE, Edit, Expr, GATE, GateRole, LoadError, Node, Outline,
    PluginInfo, ReplayError, Resolver, Starter, Workspace, apply_edits, check, flow_style,
    is_library_step_name, library_step_plugin, replay, resolve_uses, try_edits,
};
use slopwatch_protocol::pipeline::{
    DraftBase, DraftStep, NodePosition, PaletteItem, PipelineDraft, PipelinePr,
};
use slopwatch_protocol::{GateTerm, RepoName, StepInfo};
use tokio::sync::broadcast;

use crate::Library;
use crate::store::{Store, StoreError, StoredDraft};

/// Where a draft's base comes from, and where publishing sends it.
#[async_trait]
pub trait PipelineSource: Send + Sync {
    /// The Pipeline file at the tip of the repo's default branch.
    async fn default_pipeline(&self, repo: &RepoName) -> Result<BaseFile, String>;

    /// Commits `publication.text` as the Pipeline file to
    /// `slopwatch/pipeline`, branched from `publication.base`, and opens a
    /// PR from it into the base branch, or updates the one already open.
    async fn publish(
        &self,
        _repo: &RepoName,
        _publication: &Publication<'_>,
    ) -> Result<PipelinePr, String> {
        Err("This daemon can't publish Pipelines".to_owned())
    }

    /// Merges `pr`, the repo's Pipeline PR, at the head publishing left.
    async fn merge(&self, _repo: &RepoName, _pr: &PipelinePr) -> Result<Landed, String> {
        Err("This daemon can't merge Pipeline PRs".to_owned())
    }
}

/// What a Step's Plugin needs from this machine, beyond being installed.
pub trait StepNeeds: Send + Sync {
    /// The Secrets a Step of `plugin` requires that aren't set.
    fn missing_secrets(&self, plugin: &str) -> Vec<String>;
}

/// What publishing commits.
#[derive(Debug, Clone, Copy)]
pub struct Publication<'a> {
    /// The default branch, at the commit the file was replayed onto.
    pub base: &'a DraftBase,
    /// The whole Pipeline file.
    pub text: &'a str,
    /// The commit's first line, which titles a new PR too.
    pub headline: &'a str,
    pub body: &'a str,
}

/// What became of "Merge it now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    Merged,
    /// The base branch's merge queue took the PR.
    Enqueued,
}

/// A Pipeline file as it stands on a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseFile {
    pub base: DraftBase,
    /// `None` when the branch has no Pipeline file.
    pub text: Option<String>,
}

#[derive(Debug)]
pub enum DraftError {
    /// No such repo, or a node that isn't in the draft.
    NotFound(String),
    /// The edits would make the Pipeline invalid, or don't fit it. Says why.
    Refused(String),
    /// The base couldn't be read.
    Source(String),
    Store(StoreError),
}

impl From<StoreError> for DraftError {
    fn from(error: StoreError) -> Self {
        DraftError::Store(error)
    }
}

pub struct Drafts {
    store: Store,
    resolver: Arc<dyn Resolver + Send + Sync>,
    library: Arc<Library>,
    source: Arc<dyn PipelineSource>,
    /// What each Step lacks, for its badges. Without it, nothing shows.
    needs: Option<Arc<dyn StepNeeds>>,
    /// Every change to any draft. Each connection passes on its repos'.
    updates: broadcast::Sender<Arc<PipelineDraft>>,
    /// Edits read, change and write a draft, one at a time.
    writing: Mutex<()>,
    /// One publish or merge at a time. They wait on GitHub, so this
    /// isn't `writing`, which edits take.
    publishing: tokio::sync::Mutex<()>,
}

impl Drafts {
    pub fn new(
        store: Store,
        resolver: Arc<dyn Resolver + Send + Sync>,
        library: Arc<Library>,
        source: Arc<dyn PipelineSource>,
    ) -> Self {
        Self {
            store,
            resolver,
            library,
            source,
            needs: None,
            updates: broadcast::Sender::new(64),
            writing: Mutex::new(()),
            publishing: tokio::sync::Mutex::new(()),
        }
    }

    /// Shows on each Step what it lacks, as `needs` tells.
    pub fn with_needs(mut self, needs: Arc<dyn StepNeeds>) -> Self {
        self.needs = Some(needs);
        self
    }

    /// Sends every draft to its clients again, as after a Secret was set,
    /// which changes what its Steps lack.
    pub fn refresh(&self) -> Result<(), DraftError> {
        for repo in self.store.repos()? {
            if self.store.draft(&repo)?.is_some() {
                self.broadcast(&repo)?;
            }
        }
        Ok(())
    }

    /// Each draft as it changes.
    pub fn updates(&self) -> broadcast::Receiver<Arc<PipelineDraft>> {
        self.updates.subscribe()
    }

    /// The repo's draft, starting it from the default branch if it has none.
    /// A draft without edits starts over from the branch as it is now, so
    /// it follows Pipeline changes that land before the developer edits.
    /// So does a draft whose edits the branch has, as once its Pipeline PR
    /// merged.
    pub async fn open(&self, repo: &RepoName) -> Result<PipelineDraft, DraftError> {
        if !self.store.repos()?.contains(repo) {
            return Err(DraftError::NotFound(format!("Not found: repo {repo}")));
        }
        let stored = self.store.draft(repo)?;
        match self.source.default_pipeline(repo).await {
            Ok(file) => {
                if self.follow(repo, file, false)? {
                    // Other clients may show the old base.
                    self.broadcast(repo)?;
                }
            }
            // Offline, a draft already kept will do.
            Err(_) if stored.is_some() => {}
            Err(error) => return Err(source_error(repo, &error)),
        }
        self.view(repo)
    }

    /// Starts the repo's draft over from `file`, the default branch as it
    /// is now, if the draft has no edits, or only edits the branch has
    /// too, or if `discard` drops them. Discarding also forgets the Pipeline
    /// PR, so "Merge it now" can't merge what the developer threw away; the
    /// next publish finds the PR again. Returns whether a draft clients
    /// already saw changed.
    fn follow(&self, repo: &RepoName, file: BaseFile, discard: bool) -> Result<bool, DraftError> {
        let _writing = self.writing.lock().expect("no panics while writing");
        // An edit may have landed while the base was read.
        let now = self.store.draft(repo)?;
        let fresh = StoredDraft::fresh(file.base, file.text);
        let fresh = match &now {
            None => fresh,
            Some(_) if discard => fresh,
            Some(draft) if draft.edits.is_empty() => StoredDraft {
                published: draft.published.clone(),
                ..fresh
            },
            // The Pipeline PR merged, so it's done with.
            Some(draft) if branch_has_edits(draft, &fresh) => fresh,
            Some(_) => return Ok(false),
        };
        if now.as_ref() == Some(&fresh) {
            return Ok(false);
        }
        self.store.save_draft(repo, &fresh)?;
        Ok(now.is_some())
    }

    /// Publishes the repo's draft as a PR from `slopwatch/pipeline`
    /// (ADR 0007): replays its edits onto the Pipeline file as it is now on
    /// the default branch, and commits the result. A node changed on both
    /// sides stops it, and the draft keeps both versions in `conflicts`.
    /// Once published, the draft's base is the branch as the replay found
    /// it, and its edits the ones that replay applied.
    pub async fn publish(&self, repo: &RepoName, edits_seen: usize) -> Result<(), DraftError> {
        let _publishing = self.publishing.lock().await;
        let draft = self.stored(repo)?;
        if draft.edits.len() != edits_seen {
            return Err(DraftError::Refused(
                "The draft changed in the meantime. Publish it again as it is now.".to_owned(),
            ));
        }
        if draft.edits.is_empty() {
            return Err(DraftError::Refused(
                "The draft has no edits to publish.".to_owned(),
            ));
        }
        let now = self
            .source
            .default_pipeline(repo)
            .await
            .map_err(|error| source_error(repo, &error))?;
        let base_text = draft.base_text.as_deref().unwrap_or(EMPTY_PIPELINE);
        let now_text = now.text.as_deref().unwrap_or(EMPTY_PIPELINE);
        let replayed = match replay(base_text, &draft.edits, now_text) {
            Ok(replayed) => replayed,
            Err(ReplayError::Conflicts(conflicts)) => {
                let reason = ReplayError::Conflicts(conflicts.clone()).to_string();
                self.change(repo, |draft| draft.conflicts = conflicts)?;
                return Err(DraftError::Refused(format!(
                    "Publishing stopped: {reason}."
                )));
            }
            Err(error) => {
                return Err(DraftError::Refused(format!("Publishing stopped: {error}.")));
            }
        };
        if replayed.text == now_text {
            let branch = now.base.branch.clone();
            self.follow(repo, now, false)?;
            self.broadcast(repo)?;
            return Err(DraftError::Refused(format!(
                "Nothing to publish: {branch} has the draft's edits already, so the draft \
                 starts over from it."
            )));
        }
        let problems: Vec<String> = check(&replayed.text, self.resolver.as_ref())
            .iter()
            .filter(|error| !error.is_about_this_machine())
            .map(LoadError::to_string)
            .collect();
        if !problems.is_empty() {
            return Err(DraftError::Refused(format!(
                "Publishing stopped: the Pipeline wouldn't load: {}.",
                problems.join("; ")
            )));
        }
        let body = format!(
            "Changes {} in the slopwatch Pipeline editor.",
            Node::describe(&Node::touched(&replayed.edits))
        );
        let pr = self
            .source
            .publish(
                repo,
                &Publication {
                    base: &now.base,
                    text: &replayed.text,
                    headline: PUBLISH_HEADLINE,
                    body: &body,
                },
            )
            .await
            .map_err(|error| {
                DraftError::Source(format!("Couldn't publish the Pipeline of {repo}: {error}"))
            })?;
        self.change(repo, |current| {
            // Edits made while publishing were made on the old base, so the
            // draft keeps it.
            if current.edits == draft.edits && current.base == draft.base {
                current.base = now.base;
                current.base_text = now.text;
                current.edits = replayed.edits;
            }
            current.published = Some(pr);
            current.conflicts.clear();
        })
    }

    /// Merges the repo's Pipeline PR, as "Merge it now" asks. Once GitHub
    /// merged it, the draft starts over from the default branch.
    pub async fn merge(&self, repo: &RepoName) -> Result<Landed, DraftError> {
        let _publishing = self.publishing.lock().await;
        let pr = self.stored(repo)?.published.ok_or_else(|| {
            DraftError::Refused("The draft has no Pipeline PR to merge. Publish it first.".into())
        })?;
        let landed = self.source.merge(repo, &pr).await.map_err(|error| {
            DraftError::Source(format!("Couldn't merge PR #{}: {error}", pr.number))
        })?;
        if landed == Landed::Merged {
            if let Ok(file) = self.source.default_pipeline(repo).await {
                self.follow(repo, file, false)?;
            }
            // GitHub's branch may lag its answer, so the draft may keep its
            // edits until the next open. The PR is done with either way.
            self.change(repo, |draft| draft.published = None)?;
        }
        Ok(landed)
    }

    /// Drops the draft's edits and starts it over from the default branch.
    pub async fn discard(&self, repo: &RepoName) -> Result<(), DraftError> {
        self.stored(repo)?;
        let file = self
            .source
            .default_pipeline(repo)
            .await
            .map_err(|error| source_error(repo, &error))?;
        self.follow(repo, file, true)?;
        self.broadcast(repo)
    }

    /// Changes the stored draft with `change`, then sends it to clients.
    fn change(
        &self,
        repo: &RepoName,
        change: impl FnOnce(&mut StoredDraft),
    ) -> Result<(), DraftError> {
        {
            let _writing = self.writing.lock().expect("no panics while writing");
            let mut draft = self.stored(repo)?;
            change(&mut draft);
            self.store.save_draft(repo, &draft)?;
        }
        self.broadcast(repo)
    }

    /// Applies `edits` to the repo's draft, all or none, and places the
    /// nodes in `positions` they add. `edits_seen` is how many edits the
    /// draft held when the client made them: edits name Gate terms by
    /// index, so ones made on an older draft could change the wrong term.
    pub fn edit(
        &self,
        repo: &RepoName,
        edits_seen: usize,
        edits: &[Edit],
        positions: &BTreeMap<String, NodePosition>,
    ) -> Result<(), DraftError> {
        self.commit(repo, edits_seen, positions, |text| {
            let after = try_edits(text, edits, self.resolver.as_ref())
                .map_err(|refusal| DraftError::Refused(refusal.to_string()))?;
            Ok((edits.to_vec(), after))
        })
    }

    /// Replaces the draft's Steps and Gate with the Starter `key`'s, as
    /// one change, and lays the canvas out afresh. A Plugin or Library Step
    /// this machine lacks doesn't refuse it, unlike a single Step dropped
    /// from the palette: onboarding shows it on the Step instead.
    pub fn apply_starter(
        &self,
        repo: &RepoName,
        edits_seen: usize,
        key: &str,
    ) -> Result<(), DraftError> {
        let starter = Starter::named(key)
            .ok_or_else(|| DraftError::NotFound(format!("Not found: a Starter named `{key}`")))?;
        self.commit(repo, edits_seen, &BTreeMap::new(), |text| {
            let outline = Outline::parse(text).map_err(DraftError::Refused)?;
            let edits = outline.use_starter(starter);
            let after = apply_edits(text, &edits)
                .map_err(|error| DraftError::Refused(error.to_string()))?;
            let resolver = self.resolver.as_ref();
            let before = check(text, resolver);
            let gained: Vec<String> = check(&after, resolver)
                .into_iter()
                .filter(|error| !error.is_about_this_machine() && !before.contains(error))
                .map(|error| error.to_string())
                .collect();
            if !gained.is_empty() {
                return Err(DraftError::Refused(gained.join("\n")));
            }
            self.store.clear_positions(repo)?;
            Ok((edits, after))
        })
    }

    /// Changes the repo's draft by the edits `make` returns for its text,
    /// with the text they make, under the edit lock. Refuses edits made on
    /// an older draft, and places the nodes in `positions` they add.
    fn commit(
        &self,
        repo: &RepoName,
        edits_seen: usize,
        positions: &BTreeMap<String, NodePosition>,
        make: impl FnOnce(&str) -> Result<(Vec<Edit>, String), DraftError>,
    ) -> Result<(), DraftError> {
        {
            let _writing = self.writing.lock().expect("no panics while writing");
            let mut draft = self.stored(repo)?;
            if draft.edits.len() != edits_seen {
                return Err(DraftError::Refused(
                    "The draft changed in the meantime. Try again on the draft as it is now."
                        .to_owned(),
                ));
            }
            let text = draft_text(&draft).map_err(DraftError::Refused)?;
            let (edits, after) = make(&text)?;
            for node in positions.keys() {
                known_node(repo, &after, node)?;
            }
            draft.edits.extend(edits.iter().cloned());
            self.store.save_draft(repo, &draft)?;
            for edit in &edits {
                if let Edit::RemoveStep { id } = edit {
                    self.store.remove_position(repo, id)?;
                }
            }
            for (node, position) in positions {
                self.store.set_position(repo, node, *position)?;
            }
        }
        self.broadcast(repo)
    }

    /// Keeps `node`, a Step id or `gate`, at `position`.
    pub fn move_node(
        &self,
        repo: &RepoName,
        node: &str,
        position: NodePosition,
    ) -> Result<(), DraftError> {
        {
            let _writing = self.writing.lock().expect("no panics while writing");
            let draft = self.stored(repo)?;
            let text = draft_text(&draft).map_err(DraftError::Refused)?;
            known_node(repo, &text, node)?;
            self.store.set_position(repo, node, position)?;
        }
        self.broadcast(repo)
    }

    /// Forgets every node position, so the canvas lays the Pipeline out
    /// again.
    pub fn tidy(&self, repo: &RepoName) -> Result<(), DraftError> {
        {
            let _writing = self.writing.lock().expect("no panics while writing");
            self.stored(repo)?;
            self.store.clear_positions(repo)?;
        }
        self.broadcast(repo)
    }

    fn stored(&self, repo: &RepoName) -> Result<StoredDraft, DraftError> {
        self.store
            .draft(repo)?
            .ok_or_else(|| DraftError::NotFound(format!("Not found: a draft Pipeline for {repo}")))
    }

    /// Sends the repo's draft to every client that follows it.
    fn broadcast(&self, repo: &RepoName) -> Result<(), DraftError> {
        let draft = self.view(repo)?;
        // Nobody listening is fine.
        let _ = self.updates.send(Arc::new(draft));
        Ok(())
    }

    /// The draft as clients see it.
    pub fn view(&self, repo: &RepoName) -> Result<PipelineDraft, DraftError> {
        let draft = self.stored(repo)?;
        let resolver = self.resolver.as_ref();
        let (text, mut problems) = match draft_text(&draft) {
            Ok(text) => {
                let problems = check(&text, resolver)
                    .iter()
                    .map(LoadError::to_string)
                    .collect();
                (text, problems)
            }
            Err(problem) => (String::new(), vec![problem]),
        };
        let outline = Outline::parse(&text).unwrap_or_else(|error| {
            problems.push(error);
            Outline::parse(EMPTY_PIPELINE).expect("the empty Pipeline parses")
        });
        let steps = outline
            .steps()
            .iter()
            .map(|step| {
                let resolved = resolve_uses(&step.uses, resolver);
                let (plugin, info) = match resolved {
                    Some((plugin, info)) => (plugin, Some(info)),
                    None => (step.uses.clone(), None),
                };
                let missing_secrets = match (&self.needs, info) {
                    (Some(needs), Some(_)) => needs.missing_secrets(&plugin),
                    _ => Vec::new(),
                };
                let missing_plugin = match info {
                    Some(_) => None,
                    None => self.missing_plugin(&step.uses),
                };
                DraftStep {
                    missing_secrets,
                    missing_plugin,
                    merge: is_merge(&plugin, info),
                    info: StepInfo {
                        id: step.id.clone(),
                        plugin,
                        needs: step.needs.clone(),
                        gated: outline.gate_role(&step.id) != GateRole::Advisory,
                        write: info.is_some_and(|info| info.workspace == Workspace::Write),
                        condition: step.when.as_ref().map(flow_style),
                    },
                    uses: step.uses.clone(),
                    with: step.with.clone(),
                }
            })
            .collect::<Vec<_>>();
        let gate_terms = outline.gate().iter().map(gate_term).collect();
        let positions: BTreeMap<String, NodePosition> = self
            .store
            .positions(repo)?
            .into_iter()
            .filter(|(node, _)| node == GATE || outline.step(node).is_some())
            .collect();
        Ok(PipelineDraft {
            repo: repo.clone(),
            base: draft.base,
            text,
            edits: draft.edits,
            steps,
            gate_terms,
            problems,
            positions,
            palette: self.palette(),
            published: draft.published,
            conflicts: draft.conflicts,
        })
    }

    /// The Plugin a Step that `uses` this runs, if this machine doesn't
    /// have it. `None` when the Plugin is there, or when a Library Step it
    /// names is missing or broken, which the draft's problems say.
    fn missing_plugin(&self, uses: &str) -> Option<String> {
        let plugin = match uses.strip_prefix("lib/") {
            Some(name) if is_library_step_name(name) => {
                library_step_plugin(&self.library.step(name)?)?
            }
            Some(_) => return None,
            None => uses.to_owned(),
        };
        self.resolver.plugin(&plugin).is_none().then_some(plugin)
    }

    /// The Library Steps that load, then the installed built-in Plugins.
    /// A Library Step whose Plugin isn't installed still shows, so the
    /// developer sees what they have; adding it is refused with the reason.
    fn palette(&self) -> Vec<PaletteItem> {
        let resolver = self.resolver.as_ref();
        let item = |uses: String, plugin: String, summary: Option<String>| {
            let info = resolver.plugin(&plugin);
            PaletteItem {
                merge: is_merge(&plugin, info),
                write: info.is_some_and(|info| info.workspace == Workspace::Write),
                installed: info.is_some(),
                uses,
                plugin,
                summary,
            }
        };
        let library = self.library.list().unwrap_or_default();
        let steps = library.into_iter().filter_map(|step| {
            let plugin = library_step_plugin(&step.text)?;
            Some(item(
                format!("lib/{}", step.name),
                plugin,
                summary(&step.text),
            ))
        });
        let plugins = BUILTIN_PLUGINS
            .iter()
            .filter(|name| resolver.plugin(name).is_some_and(|info| info.builtin))
            .map(|name| item((*name).to_owned(), (*name).to_owned(), None));
        steps.chain(plugins).collect()
    }
}

/// Fails unless `node` is `gate` or a Step of the Pipeline `text`.
fn known_node(repo: &RepoName, text: &str, node: &str) -> Result<(), DraftError> {
    if node == GATE || Outline::parse(text).is_ok_and(|outline| outline.step(node).is_some()) {
        Ok(())
    } else {
        Err(DraftError::NotFound(format!(
            "Not found: `{node}` in the draft Pipeline of {repo}"
        )))
    }
}

/// Whether a Step running `plugin` is a Merge Step.
fn is_merge(plugin: &str, info: Option<PluginInfo>) -> bool {
    plugin == "merge" && info.is_some_and(|info| info.builtin)
}

/// The draft's file: its base with every edit applied.
fn draft_text(draft: &StoredDraft) -> Result<String, String> {
    let base = draft.base_text.as_deref().unwrap_or(EMPTY_PIPELINE);
    apply_edits(base, &draft.edits).map_err(|error| error.to_string())
}

/// Whether the branch file in `fresh` has every edit of `draft` already, as
/// once the draft's Pipeline PR merged, so publishing would change nothing.
fn branch_has_edits(draft: &StoredDraft, fresh: &StoredDraft) -> bool {
    let base = draft.base_text.as_deref().unwrap_or(EMPTY_PIPELINE);
    let now = fresh.base_text.as_deref().unwrap_or(EMPTY_PIPELINE);
    replay(base, &draft.edits, now).is_ok_and(|replayed| replayed.text == now)
}

/// The commit publishing makes, and the title of the PR it opens.
const PUBLISH_HEADLINE: &str = "Update the slopwatch Pipeline";

fn source_error(repo: &RepoName, error: &str) -> DraftError {
    DraftError::Source(format!("Can't read the Pipeline of {repo}: {error}"))
}

fn gate_term(term: &Value) -> GateTerm {
    match Expr::parse_gate_term(term) {
        Ok(expr) => GateTerm::from(&expr),
        Err(_) => GateTerm::Other {
            text: flow_style(term),
        },
    }
}

/// The first sentence of a Library Step's leading comment, after the
/// "Shipped preset." the presets open with.
fn summary(text: &str) -> Option<String> {
    let comment: Vec<&str> = text
        .lines()
        .map_while(|line| line.strip_prefix('#'))
        .map(str::trim)
        .collect();
    let comment = comment.join(" ");
    let comment = comment
        .strip_prefix("Shipped preset.")
        .unwrap_or(&comment)
        .trim();
    let sentence = match comment.find(". ") {
        Some(end) => &comment[..=end],
        None => comment,
    };
    (!sentence.is_empty()).then(|| sentence.to_owned())
}
