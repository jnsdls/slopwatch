//! The Pipeline editor's draft, as the daemon keeps it and sends it on the
//! `pipeline/<owner>/<name>` topic (ADR 0007). Edits collect in the draft,
//! and publishing replays them onto the file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use slopwatch_core::{Conflict, Edit};

use crate::{GateTerm, RepoName, StepInfo};

/// A repo's draft Pipeline, whole. Each frame on its topic carries all of
/// it, so there are no deltas to apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineDraft {
    pub repo: RepoName,
    pub base: DraftBase,
    /// The file as publishing would write it: the base with every edit
    /// applied.
    pub text: String,
    /// The edits since the base, oldest first. Publishing replays these.
    pub edits: Vec<Edit>,
    /// The Steps in file order. Steps that don't resolve still show, with
    /// what they name in `plugin`.
    pub steps: Vec<DraftStep>,
    pub gate_terms: Vec<GateTerm>,
    /// Why the draft wouldn't load as it stands, one reason each. A new
    /// Pipeline starts with an empty Gate, so it starts with one.
    pub problems: Vec<String>,
    /// Where the developer put nodes, by Step id or `gate`. Nodes without
    /// one are auto-laid-out. Kept by the daemon, never in the repo.
    pub positions: BTreeMap<String, NodePosition>,
    /// What the editor can add: the developer's Library Steps and the
    /// installed Plugins.
    pub palette: Vec<PaletteItem>,
    /// The Pipeline PR the draft was last published as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<PipelinePr>,
    /// The nodes that stopped the last publish: changed on the branch since
    /// the draft started, and edited in the draft too. Empty once a publish
    /// goes through or the draft starts over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
}

impl PipelineDraft {
    pub fn step(&self, id: &str) -> Option<&DraftStep> {
        self.steps.iter().find(|step| step.info.id == id)
    }
}

/// Where a draft started: the Pipeline file on the repo's default branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftBase {
    pub branch: String,
    /// The branch's head commit when the draft started.
    pub commit: String,
    /// The Pipeline file's blob SHA at that commit. `None` when the branch
    /// had no Pipeline file, and the draft started empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// One Step of a draft. `info.condition` holds the Condition the file
/// wrote, as flow YAML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftStep {
    pub info: StepInfo,
    /// What the file writes in `uses:`.
    pub uses: String,
    /// The Step's own `with:` overrides, as the file writes them.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub with: Map<String, Value>,
    /// The Step is a Merge Step, so it belongs after the Gate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
    /// Secrets its Plugin requires that aren't set. The Step would error
    /// on them, and the editor shows a badge to paste each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_secrets: Vec<String>,
    /// The Plugin it runs, when this machine doesn't have it installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_plugin: Option<String>,
}

/// A PR from the `slopwatch/pipeline` branch, which publishing opens and
/// then updates (ADR 0007).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelinePr {
    pub number: u64,
    pub url: String,
    /// The branch's head after the last publish.
    pub head: String,
}

/// A node's place on the canvas, in canvas pixels from its top left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodePosition {
    pub x: i32,
    pub y: i32,
}

/// Something the editor's palette can add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteItem {
    /// What the new Step writes in `uses:`: `lib/<name>` or a Plugin name.
    pub uses: String,
    /// The Plugin it runs.
    pub plugin: String,
    /// It declares `workspace: write`, so it's terminal and goes after the
    /// Gate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub write: bool,
    /// It's a Merge Step, which goes after the Gate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
    /// Its Plugin is installed. A Library Step whose Plugin isn't can't be
    /// added until it is.
    pub installed: bool,
    /// The first sentence of a Library Step's leading comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl PaletteItem {
    /// A Step that must run after the Gate: a Merge Step, or a write Step,
    /// which is terminal (ADR 0001).
    pub fn after_gate(&self) -> bool {
        self.write || self.merge
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Topic};
    use serde_json::json;

    #[test]
    fn a_pipeline_topic_names_its_repo() {
        let topic: Topic = serde_json::from_value(json!("pipeline/jnsdls/slopwatch")).unwrap();

        assert_eq!(topic, Topic::Pipeline(RepoName::new("jnsdls", "slopwatch")));
        assert_eq!(topic.to_string(), "pipeline/jnsdls/slopwatch");
        assert!(serde_json::from_value::<Topic>(json!("pipeline/nope")).is_err());
    }

    #[test]
    fn editing_commands_carry_core_edits_and_canvas_positions() {
        let edit = Command::EditPipeline {
            repo: RepoName::new("o", "r"),
            edits_seen: 3,
            edits: vec![Edit::AddStep {
                id: "lint".into(),
                step: serde_json::Map::from_iter([("uses".into(), json!("ci"))]),
            }],
            positions: BTreeMap::from([("lint".into(), NodePosition { x: 10, y: 20 })]),
        };
        let moved = Command::MovePipelineNode {
            repo: RepoName::new("o", "r"),
            node: "gate".into(),
            position: NodePosition { x: 240, y: 30 },
        };

        assert_eq!(
            serde_json::to_value(&edit).unwrap(),
            json!({
                "name": "edit_pipeline",
                "repo": "o/r",
                "edits_seen": 3,
                "edits": [{ "add_step": { "id": "lint", "step": { "uses": "ci" } } }],
                "positions": { "lint": { "x": 10, "y": 20 } },
            })
        );
        assert_eq!(
            serde_json::to_value(&moved).unwrap(),
            json!({
                "name": "move_pipeline_node",
                "repo": "o/r",
                "node": "gate",
                "position": { "x": 240, "y": 30 },
            })
        );
        assert_eq!(
            serde_json::to_value(Command::TidyPipeline {
                repo: RepoName::new("o", "r")
            })
            .unwrap(),
            json!({ "name": "tidy_pipeline", "repo": "o/r" })
        );
    }

    #[test]
    fn publishing_commands_name_the_repo() {
        let repo = || RepoName::new("o", "r");
        let wire = |command: Command| serde_json::to_value(command).unwrap();

        assert_eq!(
            wire(Command::PublishPipeline {
                repo: repo(),
                edits_seen: 2
            }),
            json!({ "name": "publish_pipeline", "repo": "o/r", "edits_seen": 2 })
        );
        assert_eq!(
            wire(Command::MergePipeline { repo: repo() }),
            json!({ "name": "merge_pipeline", "repo": "o/r" })
        );
        assert_eq!(
            wire(Command::DiscardPipelineDraft { repo: repo() }),
            json!({ "name": "discard_pipeline_draft", "repo": "o/r" })
        );
    }

    #[test]
    fn a_draft_from_before_publishing_reads_as_unpublished() {
        let draft = json!({
            "repo": "o/r",
            "base": { "branch": "main", "commit": "c1" },
            "text": "version: 1\n",
            "edits": [],
            "steps": [],
            "gate_terms": [],
            "problems": [],
            "positions": {},
            "palette": [],
        });

        let draft: PipelineDraft = serde_json::from_value(draft.clone()).unwrap();

        assert_eq!(draft.published, None);
        assert!(draft.conflicts.is_empty());
        let published = PipelineDraft {
            published: Some(PipelinePr {
                number: 4,
                url: "https://github.com/o/r/pull/4".into(),
                head: "abc".into(),
            }),
            conflicts: vec![Conflict {
                node: slopwatch_core::Node::Gate,
                draft: Some("gate: [ci]".into()),
                branch: None,
            }],
            ..draft
        };
        let wire = serde_json::to_value(&published).unwrap();
        assert_eq!(
            wire["published"],
            json!({ "number": 4, "url": "https://github.com/o/r/pull/4", "head": "abc" })
        );
        assert_eq!(
            wire["conflicts"],
            json!([{ "node": "gate", "draft": "gate: [ci]", "branch": null }])
        );
    }
}
