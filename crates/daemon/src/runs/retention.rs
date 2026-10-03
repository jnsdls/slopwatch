//! Which Runs lose their detail. A Run's record (its row, Outcomes and
//! summary) is kept forever. Its detail, the Step logs and most of its
//! event journal, is pruned 14 days after the Run ends, or sooner when all
//! detail together takes more than the 2 GB cap: oldest first, closed PRs'
//! Runs before open ones'.
//!
//! The latest Run of every open Watched PR is protected and keeps its
//! detail whatever its age, as does every Run still going, and every Run
//! whose Outcome a kept Run reuses, so the reused Step's log stays. The cap is soft
//! for them: if protected detail alone goes over it, nothing more is pruned
//! and the daemon raises the storage warning instead.

use std::time::Duration;

use slopwatch_protocol::{RunId, StorageWarning};

/// The retention settings.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// How long detail stays after its Run ends.
    pub keep_for: Duration,
    /// How much detail may take in all.
    pub cap_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_for: Duration::from_secs(14 * 24 * 60 * 60),
            cap_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// One Run whose detail is still kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub run: RunId,
    /// Seconds since the epoch. `None` while the Run is going.
    pub ended_at: Option<i64>,
    /// The latest Run of an open Watched PR.
    pub latest_of_watched: bool,
    /// A Run whose detail is kept reuses one of this Run's Outcomes.
    pub reused: bool,
    /// The Run's PR is no longer among the developer's open PRs.
    pub pr_closed: bool,
    pub bytes: u64,
}

impl Detail {
    fn protected(&self) -> bool {
        self.ended_at.is_none() || self.latest_of_watched || self.reused
    }
}

/// What to prune now, at `now` in seconds, and the warning to show after.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub prune: Vec<RunId>,
    pub warning: Option<StorageWarning>,
}

pub fn plan(details: &[Detail], retention: Retention, now: i64) -> Plan {
    let keep_for = retention.keep_for.as_secs() as i64;
    let mut plan = Plan::default();
    let mut kept: Vec<&Detail> = Vec::new();
    for detail in details {
        let expired = detail
            .ended_at
            .is_some_and(|ended| ended.saturating_add(keep_for) <= now);
        if expired && !detail.protected() {
            plan.prune.push(detail.run);
        } else {
            kept.push(detail);
        }
    }

    let mut used: u64 = kept.iter().map(|detail| detail.bytes).sum();
    let mut evictable: Vec<&Detail> = kept
        .iter()
        .copied()
        .filter(|detail| !detail.protected())
        .collect();
    // Closed PRs' Runs first, then the oldest.
    evictable.sort_by_key(|detail| (!detail.pr_closed, detail.ended_at, detail.run));
    for detail in evictable {
        if used <= retention.cap_bytes {
            break;
        }
        plan.prune.push(detail.run);
        used -= detail.bytes;
    }
    if used > retention.cap_bytes {
        plan.warning = Some(StorageWarning {
            used_bytes: used,
            cap_bytes: retention.cap_bytes,
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60;

    fn detail(run: u64, ended_at: Option<i64>, bytes: u64) -> Detail {
        Detail {
            run: RunId(run),
            ended_at,
            latest_of_watched: false,
            reused: false,
            pr_closed: false,
            bytes,
        }
    }

    fn retention(cap_bytes: u64) -> Retention {
        Retention {
            cap_bytes,
            ..Retention::default()
        }
    }

    fn ids(runs: &[RunId]) -> Vec<u64> {
        runs.iter().map(|run| run.0).collect()
    }

    #[test]
    fn detail_goes_14_days_after_its_run_ends() {
        let now = 100 * DAY;
        let details = [
            detail(1, Some(now - 15 * DAY), 10),
            detail(2, Some(now - 14 * DAY), 10),
            detail(3, Some(now - 13 * DAY), 10),
            detail(4, None, 10),
        ];

        let plan = plan(&details, retention(u64::MAX), now);

        assert_eq!(ids(&plan.prune), [1, 2]);
        assert_eq!(plan.warning, None);
    }

    #[test]
    fn the_latest_run_of_an_open_watched_pr_keeps_its_detail_whatever_its_age() {
        let now = 100 * DAY;
        let details = [Detail {
            latest_of_watched: true,
            ..detail(1, Some(0), 10)
        }];

        assert!(plan(&details, retention(u64::MAX), now).prune.is_empty());
    }

    #[test]
    fn a_run_whose_outcome_a_kept_run_reuses_keeps_its_detail() {
        let now = 100 * DAY;
        let details = [Detail {
            reused: true,
            ..detail(1, Some(0), 10)
        }];

        assert!(plan(&details, retention(0), now).prune.is_empty());
    }

    #[test]
    fn over_the_cap_closed_prs_go_first_then_the_oldest() {
        let now = 10 * DAY;
        let details = [
            detail(1, Some(now - 3 * DAY), 40),
            Detail {
                pr_closed: true,
                ..detail(2, Some(now - DAY), 40)
            },
            detail(3, Some(now - 2 * DAY), 40),
            detail(4, Some(now - 5 * DAY), 40),
        ];

        let plan = plan(&details, retention(70), now);

        assert_eq!(ids(&plan.prune), [2, 4, 1]);
        assert_eq!(plan.warning, None);
    }

    #[test]
    fn protected_detail_alone_over_the_cap_raises_the_warning() {
        let now = 10 * DAY;
        let details = [
            detail(1, None, 80),
            Detail {
                latest_of_watched: true,
                ..detail(2, Some(now), 80)
            },
            detail(3, Some(now), 5),
        ];

        let plan = plan(&details, retention(100), now);

        assert_eq!(ids(&plan.prune), [3]);
        assert_eq!(
            plan.warning,
            Some(StorageWarning {
                used_bytes: 160,
                cap_bytes: 100
            })
        );
    }
}
