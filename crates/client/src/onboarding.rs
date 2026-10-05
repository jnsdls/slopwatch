//! Onboarding a repo (Prototype: onboarding a repo, variant D): the
//! coach-mark tour over the Pipeline editor, and where its card goes. A
//! repo added without a Pipeline opens on its empty canvas with the tour; a
//! repo that has one skips straight to watching. No GPUI here, so it tests
//! without a window.
//!
//! The tour walks Stops in order. A Stop that asks the developer to do
//! something moves on once it's done; one that only explains has Next.
//! Which Stops there are follows the draft: Fix only with a write Step, and
//! one about what's missing only while a Step lacks a Secret or a Plugin.

use slopwatch_protocol::pipeline::{DraftStep, PipelineDraft};
use slopwatch_protocol::{PrStatus, PullRequest};

use crate::run_graph::{Point, Rect};

/// Where an added repo goes once its draft arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// No Pipeline on the default branch: the empty canvas and the tour.
    Tour,
    /// It has a Pipeline already, so the developer goes straight to its
    /// PRs.
    Watching,
}

impl Landing {
    pub fn for_draft(draft: &PipelineDraft) -> Landing {
        if draft.base.blob.is_some() {
            Landing::Watching
        } else {
            Landing::Tour
        }
    }
}

/// What a Step lacks on this machine, which its red badge shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lack {
    /// A Secret its Plugin requires isn't set. The badge takes it.
    Secret(String),
    /// Its Plugin isn't installed.
    Plugin(String),
}

impl Lack {
    /// The first thing `step` lacks, a Secret before its Plugin.
    pub fn of(step: &DraftStep) -> Option<Lack> {
        let secret = step.missing_secrets.first().cloned().map(Lack::Secret);
        secret.or_else(|| step.missing_plugin.clone().map(Lack::Plugin))
    }
}

/// One stop of the tour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Pick a Starter from the palette.
    Starter,
    /// What the Gate is.
    Gate,
    /// What the write Step `0` does.
    Fix(String),
    /// Step `step` lacks something.
    Lacks { step: String, lack: Lack },
    /// Publish the draft as a PR.
    Publish,
    /// Pick PRs to watch.
    Watch,
    /// Where the first Run shows.
    FirstRun,
}

/// What a Stop's spotlight lands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// The palette's Starters.
    Starters,
    Gate,
    Step(String),
    /// No spotlight: the card sits in the middle.
    Center,
}

impl Stop {
    pub fn anchor(&self) -> Anchor {
        match self {
            Stop::Starter => Anchor::Starters,
            Stop::Gate => Anchor::Gate,
            Stop::Fix(step) | Stop::Lacks { step, .. } => Anchor::Step(step.clone()),
            Stop::Publish | Stop::Watch | Stop::FirstRun => Anchor::Center,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Stop::Starter => "Start from a Starter".into(),
            Stop::Gate => "This is the Gate".into(),
            Stop::Fix(_) => "Fix runs when the Gate fails".into(),
            Stop::Lacks {
                lack: Lack::Secret(_),
                ..
            } => "This Step needs a Secret".into(),
            Stop::Lacks {
                lack: Lack::Plugin(_),
                ..
            } => "This Step's Plugin isn't installed".into(),
            Stop::Publish => "Put it in the repo".into(),
            Stop::Watch => "Pick PRs to watch".into(),
            Stop::FirstRun => "Your first Run".into(),
        }
    }

    /// What the card says. `branch` is the repo's default branch.
    pub fn body(&self, branch: &str) -> String {
        match self {
            Stop::Starter => "Each Starter is a Pipeline: Steps, the order they run in, and the \
                 Gate. Pick the closest one. You can add or remove Library Steps below it \
                 anytime."
                .into(),
            Stop::Gate => "The PR is shippable when every Step listed here passes. Dashed \
                 Steps aren't in the Gate: they advise, and their Findings still reach Fix."
                .into(),
            Stop::Fix(_) => "It reads the Findings behind the failing terms and commits a fix. \
                 That commit ends the Run and starts a new one, up to 3 rounds."
                .into(),
            Stop::Lacks {
                lack: Lack::Secret(secret),
                ..
            } => format!(
                "It needs the Secret {secret}. Click the red badge on the Step to paste it. The \
                 daemon keeps it in the Keychain and never shows it back. You can publish \
                 without it: the PRs that need it wait until it's set."
            ),
            Stop::Lacks {
                lack: Lack::Plugin(plugin),
                ..
            } => format!(
                "It runs the {plugin} Plugin, which this Mac doesn't have. Install it, or remove \
                 the Step. You can publish without it: the Step errors until the Plugin is there."
            ),
            Stop::Publish => format!(
                "slopwatch opens a PR that adds .slopwatch/pipeline.yml. Runs read the \
                 Pipeline from {branch}, so nothing runs until it merges."
            ),
            Stop::Watch => format!(
                "Watching adds the slopwatch label. PRs you watch before the Pipeline is on \
                 {branch} wait for it, then start."
            ),
            Stop::FirstRun => "Each Watched PR gets a Run once the Pipeline is on its base \
                 branch. Follow it in the PR list, where Graph shows each Step as it settles. \
                 Reopen this tour from the ? in the toolbar."
                .into(),
        }
    }

    /// The Stop asks the developer to do something, and moves on once
    /// that's done. Otherwise it only explains, and Next moves on.
    fn acts(&self) -> bool {
        matches!(self, Stop::Starter | Stop::Lacks { .. } | Stop::Publish)
    }

    /// Whether the thing this Stop asks for is done in `draft`.
    pub fn done(&self, draft: &PipelineDraft) -> bool {
        match self {
            Stop::Starter => !draft.steps.is_empty(),
            Stop::Lacks { .. } => first_lack(draft).is_none(),
            Stop::Publish => draft.published.is_some() || pipeline_landed(draft),
            _ => false,
        }
    }

    /// The button that moves on, if any. An acting Stop has one once it's
    /// done, as after going back to it; until then only the Stop that
    /// can't block publishing offers to move on.
    pub fn next_label(&self, last: bool, done: bool) -> Option<&'static str> {
        match self {
            _ if last => Some("Done"),
            Stop::Lacks { .. } if !done => Some("Later"),
            Stop::Starter | Stop::Publish if !done => None,
            _ => Some("Next"),
        }
    }
}

/// The draft's first Step that lacks something, with what it lacks.
fn first_lack(draft: &PipelineDraft) -> Option<(String, Lack)> {
    draft
        .steps
        .iter()
        .find_map(|step| Some((step.info.id.clone(), Lack::of(step)?)))
}

/// The Pipeline is on the default branch, as the draft's base.
fn pipeline_landed(draft: &PipelineDraft) -> bool {
    draft.base.blob.is_some() && draft.edits.is_empty()
}

/// The tour over one repo's editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tour {
    on: bool,
    at: usize,
    /// The furthest Stop reached. Going back to a Stop that's done doesn't
    /// skip forward past it again.
    furthest: usize,
    /// The Stop about what's missing, as it last showed. It stays once
    /// nothing is, so the Stops after it keep their places.
    lacks: Option<Stop>,
}

impl Tour {
    /// Opens the tour at its first Stop.
    pub fn start(&mut self) {
        *self = Tour {
            on: true,
            ..Tour::default()
        };
    }

    /// Closes it. The `?` in the toolbar starts it again.
    pub fn skip(&mut self) {
        self.on = false;
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// The Stops for the draft as it stands.
    pub fn stops(&self, draft: &PipelineDraft) -> Vec<Stop> {
        let mut stops = vec![Stop::Starter, Stop::Gate];
        if let Some(step) = draft.steps.iter().find(|step| step.info.write) {
            stops.push(Stop::Fix(step.info.id.clone()));
        }
        match first_lack(draft) {
            Some((step, lack)) => stops.push(Stop::Lacks { step, lack }),
            None => stops.extend(self.lacks.clone()),
        }
        stops.extend([Stop::Publish, Stop::Watch, Stop::FirstRun]);
        stops
    }

    /// The Stop showing now, as its place and the count, after moving past
    /// every acting Stop that's done at the furthest point reached. `None`
    /// while the tour is off.
    pub fn current(&mut self, draft: &PipelineDraft) -> Option<(usize, usize, Stop)> {
        if !self.on {
            return None;
        }
        let stops = self.stops(draft);
        let last = stops.len() - 1;
        self.at = self.at.min(last);
        while self.at < last
            && self.at >= self.furthest
            && stops[self.at].acts()
            && stops[self.at].done(draft)
        {
            self.at += 1;
        }
        self.furthest = self.furthest.max(self.at);
        let stop = stops[self.at].clone();
        if matches!(stop, Stop::Lacks { .. }) {
            self.lacks = Some(stop.clone());
        }
        Some((self.at, stops.len(), stop))
    }

    /// Moves on, or closes the tour after its last Stop.
    pub fn next(&mut self, draft: &PipelineDraft) {
        if self.at + 1 >= self.stops(draft).len() {
            self.on = false;
        } else {
            self.at += 1;
        }
    }

    pub fn back(&mut self) {
        self.at = self.at.saturating_sub(1);
    }
}

/// What a PR on the Watch Stop's list waits for, if anything.
pub fn watch_line(pr: &PullRequest) -> Option<&'static str> {
    match pr.status {
        PrStatus::Waiting => Some("waits for the Pipeline"),
        PrStatus::Ready | PrStatus::NotWatched => None,
    }
}

/// The part of `a` inside `b`, if any.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// How far the card keeps from its anchor and the edges.
const GAP: f32 = 16.;

/// Where a coach card of `size` goes next to `anchor`, inside `room`. It
/// tries the right, left, below and above, in that order, and takes the
/// first side with room that covers nothing in `keep_clear`, such as the
/// Steps after the Gate. When every side covers something, it takes the
/// side that covers least, and covering the anchor itself counts as worse
/// than covering any other Step. All rects share one coordinate space.
pub fn place_card(anchor: Rect, size: (f32, f32), room: Rect, keep_clear: &[Rect]) -> Point {
    let (width, height) = size;
    let sides = [
        Point {
            x: anchor.x + anchor.width + GAP,
            y: anchor.y,
        },
        Point {
            x: anchor.x - GAP - width,
            y: anchor.y,
        },
        Point {
            x: anchor.x,
            y: anchor.y + anchor.height + GAP,
        },
        Point {
            x: anchor.x,
            y: anchor.y - GAP - height,
        },
    ];
    let fits = |at: &Point| {
        at.x >= room.x
            && at.y >= room.y
            && at.x + width <= room.x + room.width
            && at.y + height <= room.y + room.height
    };
    let clamp = |at: Point| Point {
        x: at
            .x
            .min(room.x + room.width - width - GAP)
            .max(room.x + GAP),
        y: at
            .y
            .min(room.y + room.height - height - GAP)
            .max(room.y + GAP),
    };
    let covered = |at: Point| {
        let card = Rect {
            x: at.x,
            y: at.y,
            width,
            height,
        };
        let others: f32 = keep_clear.iter().map(|other| overlap(card, *other)).sum();
        // Every Step together covers less than the whole canvas.
        others + overlap(card, anchor) * (room.width * room.height)
    };
    // A side with room comes first; failing all, each side clamped into
    // the room.
    let candidates = sides
        .iter()
        .copied()
        .filter(fits)
        .chain(sides.iter().copied().map(clamp));
    let mut best: Option<(Point, f32)> = None;
    for at in candidates {
        let cover = covered(at);
        if cover == 0. {
            return at;
        }
        if best.is_none_or(|(_, least)| cover < least) {
            best = Some((at, cover));
        }
    }
    best.map_or(clamp(sides[0]), |(at, _)| at)
}

fn overlap(a: Rect, b: Rect) -> f32 {
    intersect(a, b).map_or(0., |both| both.width * both.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use slopwatch_protocol::pipeline::{DraftBase, PipelinePr};
    use slopwatch_protocol::{RepoName, StepInfo};
    use std::collections::BTreeMap;

    fn step(id: &str, write: bool, missing: &[&str]) -> DraftStep {
        DraftStep {
            info: StepInfo {
                id: id.into(),
                plugin: id.into(),
                needs: Vec::new(),
                gated: !write,
                write,
                condition: None,
            },
            uses: id.into(),
            with: Map::new(),
            merge: false,
            missing_secrets: missing.iter().map(|&name| name.into()).collect(),
            missing_plugin: None,
        }
    }

    fn draft(steps: Vec<DraftStep>) -> PipelineDraft {
        PipelineDraft {
            repo: RepoName::new("o", "r"),
            base: DraftBase {
                branch: "main".into(),
                commit: "c1".into(),
                blob: None,
            },
            text: String::new(),
            edits: Vec::new(),
            steps,
            gate_terms: Vec::new(),
            problems: Vec::new(),
            positions: BTreeMap::new(),
            palette: Vec::new(),
            published: None,
            conflicts: Vec::new(),
        }
    }

    /// What Review and fix leaves before the Jev Secret is set.
    fn picked() -> PipelineDraft {
        draft(vec![
            step("ci", false, &[]),
            step("desc-matches-diff", false, &["AI_GATEWAY_API_KEY"]),
            step("claude-review", false, &[]),
            step("claude-fix", true, &[]),
        ])
    }

    fn at(tour: &mut Tour, draft: &PipelineDraft) -> Stop {
        tour.current(draft).expect("the tour is on").2
    }

    fn lacks_secret() -> Stop {
        Stop::Lacks {
            step: "desc-matches-diff".into(),
            lack: Lack::Secret("AI_GATEWAY_API_KEY".into()),
        }
    }

    #[test]
    fn an_added_repo_without_a_pipeline_lands_on_the_tour_and_one_with_it_on_watching() {
        let mut with = draft(vec![step("ci", false, &[])]);
        with.base.blob = Some("b1".into());

        assert_eq!(Landing::for_draft(&draft(Vec::new())), Landing::Tour);
        assert_eq!(Landing::for_draft(&with), Landing::Watching);
    }

    #[test]
    fn the_tour_waits_on_a_starter_then_walks_the_gate_fix_and_the_missing_secret() {
        let mut tour = Tour::default();
        let empty = draft(Vec::new());
        assert_eq!(tour.current(&empty), None, "off until started");
        tour.start();

        assert_eq!(at(&mut tour, &empty), Stop::Starter);
        assert_eq!(Stop::Starter.anchor(), Anchor::Starters);
        assert_eq!(
            Stop::Starter.next_label(false, false),
            None,
            "picking moves on"
        );

        let picked = picked();
        assert_eq!(at(&mut tour, &picked), Stop::Gate);
        tour.next(&picked);
        assert_eq!(at(&mut tour, &picked), Stop::Fix("claude-fix".into()));
        assert_eq!(
            Stop::Fix("claude-fix".into()).anchor(),
            Anchor::Step("claude-fix".into())
        );
        tour.next(&picked);
        let lacks = at(&mut tour, &picked);
        assert_eq!(lacks, lacks_secret());
        assert_eq!(lacks.anchor(), Anchor::Step("desc-matches-diff".into()));
        assert_eq!(
            lacks.next_label(false, false),
            Some("Later"),
            "a missing Secret never blocks publishing"
        );
    }

    #[test]
    fn setting_the_secret_moves_the_tour_on_to_publishing() {
        let mut tour = Tour::default();
        tour.start();
        let picked = picked();
        at(&mut tour, &picked);
        tour.next(&picked);
        tour.next(&picked);
        at(&mut tour, &picked);

        let mut set = picked.clone();
        set.steps[1].missing_secrets.clear();

        assert_eq!(at(&mut tour, &set), Stop::Publish);
        assert_eq!(Stop::Publish.anchor(), Anchor::Center);
    }

    #[test]
    fn a_step_whose_plugin_isnt_installed_gets_a_stop_too() {
        let mut tour = Tour::default();
        tour.start();
        let mut review = step("claude-review", false, &[]);
        review.missing_plugin = Some("claude".into());
        let drafted = draft(vec![step("ci", false, &[]), review]);

        let stops = tour.stops(&drafted);

        assert_eq!(
            stops[2],
            Stop::Lacks {
                step: "claude-review".into(),
                lack: Lack::Plugin("claude".into()),
            }
        );
        assert_eq!(stops[2].title(), "This Step's Plugin isn't installed");
    }

    #[test]
    fn a_pipeline_without_a_write_step_or_anything_missing_skips_those_stops() {
        let mut tour = Tour::default();
        tour.start();
        let just_ci = draft(vec![step("ci", false, &[])]);

        assert_eq!(
            tour.stops(&just_ci),
            [
                Stop::Starter,
                Stop::Gate,
                Stop::Publish,
                Stop::Watch,
                Stop::FirstRun
            ]
        );
        assert_eq!(at(&mut tour, &just_ci), Stop::Gate);
        tour.next(&just_ci);
        assert_eq!(at(&mut tour, &just_ci), Stop::Publish);
    }

    #[test]
    fn publishing_moves_on_to_watching_and_the_last_stop_closes_the_tour() {
        let mut tour = Tour::default();
        tour.start();
        let mut published = draft(vec![step("ci", false, &[])]);
        at(&mut tour, &published);
        tour.next(&published);
        published.published = Some(PipelinePr {
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
            head: "h".into(),
        });

        assert_eq!(at(&mut tour, &published), Stop::Watch);
        tour.next(&published);
        let (place, count, last) = tour.current(&published).unwrap();
        assert_eq!((place, count, last.clone()), (4, 5, Stop::FirstRun));
        assert_eq!(last.next_label(true, false), Some("Done"));
        tour.next(&published);
        assert!(!tour.on());
    }

    #[test]
    fn going_back_to_a_stop_thats_done_stays_there_until_next() {
        let mut tour = Tour::default();
        tour.start();
        let picked = picked();
        assert_eq!(at(&mut tour, &picked), Stop::Gate);

        tour.back();

        assert_eq!(at(&mut tour, &picked), Stop::Starter, "to pick another");
        assert_eq!(Stop::Starter.next_label(false, true), Some("Next"));
        tour.next(&picked);
        assert_eq!(at(&mut tour, &picked), Stop::Gate);
    }

    #[test]
    fn skipping_closes_the_tour_and_starting_again_opens_it_at_the_first_stop() {
        let mut tour = Tour::default();
        tour.start();
        let picked = picked();
        at(&mut tour, &picked);
        tour.next(&picked);
        tour.skip();
        assert_eq!(tour.current(&picked), None);

        tour.start();
        assert_eq!(at(&mut tour, &picked), Stop::Gate, "the Starter is picked");
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn covers(at: Point, size: (f32, f32), other: Rect) -> bool {
        overlap(rect(at.x, at.y, size.0, size.1), other) > 0.
    }

    #[test]
    fn the_card_goes_right_of_its_anchor_when_there_is_room() {
        let gate = rect(400., 100., 184., 160.);
        let room = rect(0., 0., 1400., 800.);

        let at = place_card(gate, (340., 190.), room, &[]);

        assert_eq!(at, Point { x: 600., y: 100. });
    }

    #[test]
    fn on_a_narrow_window_the_card_doesnt_cover_the_steps_after_the_gate() {
        // The canvas at 900 px: the Gate, with Merge and Fix to its right.
        let room = rect(0., 0., 900., 700.);
        let gate = rect(380., 60., 184., 200.);
        let after = [rect(604., 40., 164., 78.), rect(604., 132., 164., 78.)];
        let before = [rect(8., 40., 164., 78.), rect(204., 40., 164., 78.)];
        let size = (340., 190.);
        let keep_clear: Vec<Rect> = after.iter().chain(&before).copied().collect();

        let at = place_card(gate, size, room, &keep_clear);

        for step in after {
            assert!(!covers(at, size, step), "{at:?} covers {step:?}");
        }
        assert!(!covers(at, size, gate), "{at:?} covers the Gate");
        assert!(at.x >= 0. && at.x + size.0 <= room.width, "{at:?}");
        assert!(at.y >= 0. && at.y + size.1 <= room.height, "{at:?}");
    }

    #[test]
    fn with_no_clear_side_the_card_covers_another_step_before_its_anchor() {
        // The editor at its narrowest: Fix spotlit near the right, too
        // close to the edge for the card beside it, Merge below it and the
        // Gate to its left.
        let room = rect(0., 0., 960., 560.);
        let fix = rect(460., 160., 164., 78.);
        let merge = rect(460., 252., 164., 78.);
        let gate = rect(240., 160., 184., 160.);
        let size = (340., 190.);

        let at = place_card(fix, size, room, &[fix, merge, gate]);

        assert!(!covers(at, size, fix), "{at:?} covers the anchor");
        assert!(at.x >= 0. && at.x + size.0 <= room.width, "{at:?}");
    }

    #[test]
    fn with_no_clear_side_the_card_stays_in_the_room() {
        let room = rect(0., 0., 400., 300.);
        let anchor = rect(50., 50., 300., 200.);

        let at = place_card(anchor, (340., 190.), room, &[]);

        assert!(at.x >= 0. && at.x + 340. <= 400., "{at:?}");
        assert!(at.y >= 0. && at.y + 190. <= 300., "{at:?}");
    }
}
