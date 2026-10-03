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
    BUILTIN_PLUGINS, EMPTY_PIPELINE, Edit, Expr, GATE, GateRole, LoadError, Outline, PluginInfo,
    Resolver, Workspace, apply_edits, check, flow_style, library_step_plugin, resolve_uses,
    try_edits,
};
use slopwatch_protocol::pipeline::{
    DraftBase, DraftStep, NodePosition, PaletteItem, PipelineDraft,
};
use slopwatch_protocol::{GateTerm, RepoName, StepInfo};
use tokio::sync::broadcast;

use crate::Library;
use crate::store::{Store, StoreError, StoredDraft};

/// Where a draft's base comes from.
#[async_trait]
pub trait PipelineSource: Send + Sync {
    /// The Pipeline file at the tip of the repo's default branch.
    async fn default_pipeline(&self, repo: &RepoName) -> Result<BaseFile, String>;
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
    /// Every change to any draft. Each connection passes on its repos'.
    updates: broadcast::Sender<Arc<PipelineDraft>>,
    /// Edits read, change and write a draft, one at a time.
    writing: Mutex<()>,
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
            updates: broadcast::Sender::new(64),
            writing: Mutex::new(()),
        }
    }

    /// Each draft as it changes.
    pub fn updates(&self) -> broadcast::Receiver<Arc<PipelineDraft>> {
        self.updates.subscribe()
    }

    /// The repo's draft, starting it from the default branch if it has none.
    /// A draft without edits starts over from the branch as it is now, so
    /// it follows Pipeline changes that land before the developer edits.
    pub async fn open(&self, repo: &RepoName) -> Result<PipelineDraft, DraftError> {
        if !self.store.repos()?.contains(repo) {
            return Err(DraftError::NotFound(format!("Not found: repo {repo}")));
        }
        let stored = self.store.draft(repo)?;
        if stored.as_ref().is_some_and(|draft| !draft.edits.is_empty()) {
            return self.view(repo);
        }
        match self.source.default_pipeline(repo).await {
            Ok(file) => {
                let fresh = StoredDraft {
                    base: file.base,
                    base_text: file.text,
                    edits: Vec::new(),
                };
                let rebased = {
                    let _writing = self.writing.lock().expect("no panics while writing");
                    // An edit may have landed while the base was read.
                    let now = self.store.draft(repo)?;
                    let rebase = now.as_ref().is_none_or(|draft| draft.edits.is_empty())
                        && now.as_ref() != Some(&fresh);
                    if rebase {
                        self.store.save_draft(repo, &fresh)?;
                    }
                    rebase && now.is_some()
                };
                if rebased {
                    // Other clients may show the old base.
                    self.publish(repo)?;
                }
            }
            // Offline, a draft already kept will do.
            Err(_) if stored.is_some() => {}
            Err(error) => {
                return Err(DraftError::Source(format!(
                    "Can't read the Pipeline of {repo}: {error}"
                )));
            }
        }
        self.view(repo)
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
            let after = try_edits(&text, edits, self.resolver.as_ref())
                .map_err(|refusal| DraftError::Refused(refusal.to_string()))?;
            for node in positions.keys() {
                known_node(repo, &after, node)?;
            }
            draft.edits.extend_from_slice(edits);
            self.store.save_draft(repo, &draft)?;
            for edit in edits {
                if let Edit::RemoveStep { id } = edit {
                    self.store.remove_position(repo, id)?;
                }
            }
            for (node, position) in positions {
                self.store.set_position(repo, node, *position)?;
            }
        }
        self.publish(repo)
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
        self.publish(repo)
    }

    /// Forgets every node position, so the canvas lays the Pipeline out
    /// again.
    pub fn tidy(&self, repo: &RepoName) -> Result<(), DraftError> {
        {
            let _writing = self.writing.lock().expect("no panics while writing");
            self.stored(repo)?;
            self.store.clear_positions(repo)?;
        }
        self.publish(repo)
    }

    fn stored(&self, repo: &RepoName) -> Result<StoredDraft, DraftError> {
        self.store
            .draft(repo)?
            .ok_or_else(|| DraftError::NotFound(format!("Not found: a draft Pipeline for {repo}")))
    }

    fn publish(&self, repo: &RepoName) -> Result<(), DraftError> {
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
                DraftStep {
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
        })
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
