//! Stacks (ADR 0011): what the daemon does when a Stack parent leaves.
//!
//! Each sync records the parent every watched PR is stacked on. When that
//! parent leaves the poll while the child still sits on its branch, or on
//! the branch the parent merged into, the daemon asks GitHub what became
//! of it. Until it knows, the child still counts as stacked, so its Merge
//! Step won't land it.
//!
//! A parent that merged outside a native stack gets its child retargeted
//! and updated, with the method ADR 0011 picks. Both calls go through an
//! intent journal, [`StackUpdate`], like Effects. While the row is open
//! the child gets no Run, so nothing judges the diff between the retarget
//! and the push the update makes.
//!
//! A native stack's upper layers are GitHub's to retarget and rebase. A
//! parent closed without merging raises a PR entry on the child, and the
//! daemon leaves the child where it is.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use slopwatch_core::EndReason;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::step::UpdateMethod;

use super::{Engine, Input, PrKey, pr_ref};
use crate::github::{GitHub, OpenPr, PrFate};
use crate::store::{NewStackUpdate, StackLinkRow, StackStage, StackUpdate, Store, StoreError};

/// What GitHub said became of each Stack parent the sync asked after, by
/// repo and parent number. A parent GitHub couldn't be asked about is
/// missing.
pub(super) type Fates = HashMap<PrKey, PrFate>;

/// A Stack update's GitHub calls finished, as far as they go before the
/// push.
pub(super) struct Finished {
    update: StackUpdate,
}

/// The PR entry a parent closed without merging raises on its child.
const PARENT_CLOSED: &str = "Parent closed";

/// The PR entry a Stack update GitHub refused raises on the child.
const COULDNT_UPDATE: &str = "Couldn't follow its parent";

impl Engine {
    /// Loads the Stack links, and holds the PRs whose update a restart
    /// left open, before any command can start a Run on them.
    pub(super) fn load_stacks(&mut self) -> Result<(), StoreError> {
        self.links = self
            .store
            .stack_links()?
            .into_iter()
            .map(|link| ((link.repo.clone(), link.number), link))
            .collect();
        for update in self.store.open_stack_updates()? {
            let key = (update.repo.clone(), update.number);
            self.held.insert(key, holding(&update));
        }
        Ok(())
    }

    /// The Stack parents whose fate the next sync needs: those that left
    /// the poll while their watched child still sits on their branch or
    /// on the branch they'd merge into.
    pub(super) fn parents_wanted(&self, prs: &[(RepoName, OpenPr)]) -> Vec<PrKey> {
        let mut wanted: Vec<PrKey> = prs
            .iter()
            .filter_map(|(repo, pr)| {
                let link = self.links.get(&(repo.clone(), pr.number))?;
                let ask = !link.orphaned && !link.native && pr.stack.is_none() && left(link, pr);
                ask.then(|| (repo.clone(), link.parent))
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        wanted
    }

    /// The open PR the PR is stacked on, as far as the daemon knows: the
    /// one the poll shows, or one that left the poll while the PR still
    /// sits on its branch or the branch it merged into.
    pub(super) fn stacked_on(&self, key: &PrKey, pr: &OpenPr) -> Option<u64> {
        pr.parent().or_else(|| {
            let link = self.links.get(key)?;
            left(link, pr).then_some(link.parent)
        })
    }

    /// Whether the PR's Stack parent closed without merging, which leaves
    /// it on a dead branch until the developer acts.
    pub(super) fn orphaned(&self, key: &PrKey) -> bool {
        self.links.get(key).is_some_and(|link| link.orphaned)
    }

    /// Follows every watched PR's Stack parent, and returns the PRs a
    /// Stack update holds back from a new Run. Runs before the Runs whose
    /// PR moved end, so the retarget a parent's merge calls for comes first.
    pub(super) fn sync_stacks(
        &mut self,
        open: &BTreeMap<PrKey, (RepoName, OpenPr)>,
        fates: &Fates,
    ) -> Result<HashSet<PrKey>, StoreError> {
        let watched = |key: &PrKey| open.get(key).filter(|(_, pr)| pr.labeled);

        let keys: Vec<PrKey> = self.links.keys().cloned().collect();
        for key in keys {
            let link = self.links[&key].clone();
            let Some((repo, pr)) = watched(&key) else {
                self.unlink(&key)?;
                continue;
            };
            // Still on its parent, or stacked on another PR now, which the
            // links below record.
            if pr.stack.is_some() {
                continue;
            }
            if link.orphaned {
                if pr.base != link.branch {
                    // The developer moved it off the dead branch.
                    self.unlink(&key)?;
                }
                continue;
            }
            // Moved by someone else, or a native stack GitHub restacks.
            if !left(&link, pr) || link.native {
                self.unlink(&key)?;
                continue;
            }
            match fates.get(&(repo.clone(), link.parent)) {
                // GitHub couldn't say, so the next sync asks again.
                None => {}
                // The developer moved it onto the parent's base while the
                // parent stays open.
                Some(PrFate::Open) if pr.base != link.branch => self.unlink(&key)?,
                Some(PrFate::Open) => {}
                Some(PrFate::Closed) => self.parent_closed(&key, &link)?,
                Some(PrFate::Merged { into, kept_commits }) => {
                    let update = self.store.insert_stack_update(&NewStackUpdate {
                        repo,
                        number: pr.number,
                        parent: link.parent,
                        from_base: &link.branch,
                        to_base: into,
                        head_sha: &pr.head_sha,
                        kept_commits: *kept_commits,
                    })?;
                    self.links.remove(&key);
                    self.carry_out_stack_update(update);
                }
            }
        }

        for (key, (_, pr)) in open {
            let Some(stack) = pr.stack.as_ref().filter(|_| pr.labeled) else {
                continue;
            };
            let link = StackLinkRow {
                repo: key.0.clone(),
                number: key.1,
                parent: stack.parent.number,
                branch: pr.base.clone(),
                parent_base: stack.parent.base.clone(),
                native: stack.position.is_some(),
                orphaned: false,
            };
            if self.links.get(key) != Some(&link) {
                self.store.put_stack_link(&link)?;
                self.links.insert(key.clone(), link);
            }
        }

        let mut held = HashMap::new();
        for update in self.store.open_stack_updates()? {
            let key = (update.repo.clone(), update.number);
            let Some((_, pr)) = watched(&key) else {
                self.store.close_stack_update(update.id, update.stage)?;
                continue;
            };
            if !self.updating.contains(&update.id) && !self.follow_update(&update, pr)? {
                continue;
            }
            // A crash between recording the update and ending the Run.
            if let Some(run) = self.active.remove(&key) {
                self.end_run(run, EndReason::Pushed)?;
            }
            held.insert(key, holding(&update));
        }
        let changed: HashSet<PrKey> = held
            .iter()
            .filter(|(key, message)| self.held.get(*key) != Some(*message))
            .map(|(key, _)| key.clone())
            .chain(
                self.held
                    .keys()
                    .filter(|key| !held.contains_key(*key))
                    .cloned(),
            )
            .collect();
        self.held = held;
        for (repo, number) in changed {
            self.publish(&repo, number)?;
        }
        Ok(self.held.keys().cloned().collect())
    }

    /// Moves an open Stack update along with what the poll shows of its
    /// PR, while none of its calls are under way. Returns whether it still
    /// holds the PR.
    fn follow_update(&mut self, update: &StackUpdate, pr: &OpenPr) -> Result<bool, StoreError> {
        // The developer moved it somewhere else, which ends the daemon's
        // part.
        if pr.base != update.to_base && pr.base != update.from_base {
            self.store.close_stack_update(update.id, update.stage)?;
            return Ok(false);
        }
        let moved = pr.head_sha != update.head_sha;
        if update.stage == StackStage::Pushing && moved {
            self.store.close_stack_update(update.id, StackStage::Done)?;
            return Ok(false);
        }
        // The calls start again from where the branch stands: after a
        // crash, which may have cut them short or lost the push, and after
        // a push to a PR whose update failed, which may have fixed it.
        let restart = match update.stage {
            StackStage::Failed => moved,
            StackStage::Retarget | StackStage::Update | StackStage::Pushing => !self.reconciled,
            StackStage::Done => false,
        };
        if restart {
            let stage = if pr.base == update.to_base {
                StackStage::Update
            } else {
                StackStage::Retarget
            };
            self.store
                .restart_stack_update(update.id, &pr.head_sha, stage)?;
            self.carry_out_stack_update(StackUpdate {
                head_sha: pr.head_sha.clone(),
                stage,
                reason: None,
                ..update.clone()
            });
        }
        Ok(true)
    }

    /// Forgets the PR's Stack parent.
    fn unlink(&mut self, key: &PrKey) -> Result<(), StoreError> {
        self.links.remove(key);
        self.store.remove_stack_link(&key.0, key.1)
    }

    /// Raises the child's PR entry once, and leaves the child alone.
    fn parent_closed(&mut self, key: &PrKey, link: &StackLinkRow) -> Result<(), StoreError> {
        let orphaned = StackLinkRow {
            orphaned: true,
            ..link.clone()
        };
        self.store.put_stack_link(&orphaned)?;
        self.links.insert(key.clone(), orphaned);
        let latest = self.store.latest_run_id(&key.0, key.1)?;
        let reason = format!(
            "#{} closed without merging, so this PR sits on `{}`, a branch nothing will merge. \
             Retarget it or close it.",
            link.parent, link.branch
        );
        self.inbox
            .raise_pr(pr_ref(&key.0, key.1), latest, PARENT_CLOSED, vec![reason])
    }

    /// Takes a Stack update on: the child's Run ends as pushed, and the
    /// GitHub calls run on a task of their own.
    fn carry_out_stack_update(&mut self, update: StackUpdate) {
        let key = (update.repo.clone(), update.number);
        if let Some(run) = self.active.remove(&key)
            && let Err(error) = self.end_run(run, EndReason::Pushed)
        {
            eprintln!("slopwatchd: can't end {}#{}'s Run: {error}", key.0, key.1);
        }
        self.updating.insert(update.id);
        self.held.insert(key, holding(&update));
        let github = Arc::clone(&self.github);
        let store = self.store.clone();
        let reports = self.reports.clone();
        tokio::spawn(async move {
            let update = match perform(github.as_ref(), &store, update.clone()).await {
                Ok(update) => update,
                Err(error) => {
                    eprintln!(
                        "slopwatchd: can't record Stack update {}: {error}",
                        update.id
                    );
                    update
                }
            };
            let _ = reports.send(Input::Stack(Finished { update }));
        });
    }

    /// A Stack update's calls finished. One GitHub refused leaves the
    /// child waiting for the developer, with a PR entry that says why.
    pub(super) fn on_stack_update(&mut self, finished: Finished) {
        let update = finished.update;
        self.updating.remove(&update.id);
        let key = (update.repo.clone(), update.number);
        let result = (|| {
            if update.stage == StackStage::Failed {
                let latest = self.store.latest_run_id(&key.0, key.1)?;
                let reason = update.reason.clone().unwrap_or_default();
                self.inbox
                    .raise_pr(pr_ref(&key.0, key.1), latest, COULDNT_UPDATE, vec![reason])?;
            }
            if self.held.contains_key(&key) {
                self.held.insert(key.clone(), holding(&update));
                self.publish(&key.0, key.1)?;
            }
            Ok::<_, StoreError>(())
        })();
        if let Err(error) = result {
            eprintln!(
                "slopwatchd: can't record Stack update {}: {error}",
                update.id
            );
        }
    }
}

/// Whether the child's parent left the poll while the child still sits on
/// the parent's branch, or on the branch the parent would merge into, as
/// GitHub leaves it after deleting a merged parent's branch.
fn left(link: &StackLinkRow, pr: &OpenPr) -> bool {
    pr.parent() != Some(link.parent) && (pr.base == link.branch || pr.base == link.parent_base)
}

/// What the child's row says while a Stack update holds it.
fn holding(update: &StackUpdate) -> String {
    match (&update.stage, &update.reason) {
        (StackStage::Failed, Some(reason)) => format!(
            "#{} merged, and slopwatch couldn't move this PR onto {}: {reason}. \
             It waits for a push.",
            update.parent, update.to_base
        ),
        _ => format!(
            "#{} merged. Moving this PR onto {}…",
            update.parent, update.to_base
        ),
    }
}

/// Makes the update's GitHub calls from the stage it's at, and records
/// each stage it reaches.
async fn perform(
    github: &dyn GitHub,
    store: &Store,
    mut update: StackUpdate,
) -> Result<StackUpdate, StoreError> {
    let (repo, number) = (update.repo.clone(), update.number);
    let fail = |update: &mut StackUpdate, reason: String| {
        update.stage = StackStage::Failed;
        update.reason = Some(reason);
        store.set_stack_stage(
            update.id,
            StackStage::Failed,
            None,
            update.reason.as_deref(),
        )
    };
    if update.stage == StackStage::Retarget {
        if let Err(error) = github.set_base(&repo, number, &update.to_base).await {
            let reason = format!("can't retarget it onto {}: {error}", update.to_base);
            fail(&mut update, reason)?;
            return Ok(update);
        }
        update.stage = StackStage::Update;
        store.set_stack_stage(update.id, StackStage::Update, None, None)?;
    }
    let state = match github.merge_state(&repo, number, &update.head_sha).await {
        Ok(state) => state,
        Err(error) => {
            let reason = format!("can't compare it with {}: {error}", update.to_base);
            fail(&mut update, reason)?;
            return Ok(update);
        }
    };
    // Up to date already, as after the developer merged the base in. The
    // base change starts the next Run.
    if state.behind_by == Some(0) {
        update.stage = StackStage::Done;
        update.open = false;
        store.close_stack_update(update.id, StackStage::Done)?;
        return Ok(update);
    }
    let method = if !update.kept_commits || state.requires_signatures {
        UpdateMethod::Merge
    } else {
        UpdateMethod::Rebase
    };
    update.method = Some(method);
    match github
        .update_branch(&repo, number, &update.head_sha, method)
        .await
    {
        Ok(()) => {
            update.stage = StackStage::Pushing;
            store.set_stack_stage(update.id, StackStage::Pushing, Some(method), None)?;
        }
        Err(error) => {
            let reason = format!("can't update it with {}: {error}", update.to_base);
            fail(&mut update, reason)?;
        }
    }
    Ok(update)
}
