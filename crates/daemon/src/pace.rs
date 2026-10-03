//! When to poll GitHub next. Every 30 s while something is live, every
//! 2 min when everything has settled, and slower whenever polling would
//! spend more than a quarter of the hourly GraphQL budget, which the
//! daemon shares with every other `gh` call the developer makes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::github::{GitHubError, RateLimit};

pub const LIVE_INTERVAL: Duration = Duration::from_secs(30);
pub const SETTLED_INTERVAL: Duration = Duration::from_secs(120);

/// The share of the hourly budget polling may spend.
const SHARE: f64 = 0.25;
const HOUR: Duration = Duration::from_secs(3600);
/// GitHub's GraphQL budget for a user token, until a poll reports one.
const DEFAULT_LIMIT: u32 = 5000;
/// Below this share of the hour's budget left, polling waits for the reset
/// so the developer's own `gh` calls keep working.
const RESERVE: f64 = 0.05;

#[derive(Debug, Default)]
pub struct Pace {
    /// What each poll in the last hour cost, oldest first.
    spent: VecDeque<(Instant, u32)>,
    limit: Option<u32>,
    hold_until: Option<Instant>,
}

impl Pace {
    /// Records a poll that went through.
    pub fn record(&mut self, now: Instant, rate: RateLimit) {
        self.spent.push_back((now, rate.cost));
        self.limit = Some(rate.limit);
        let reserve = (f64::from(rate.limit) * RESERVE) as u32;
        self.hold_until = (rate.remaining < reserve).then(|| now + rate.resets_in);
    }

    /// Records a poll that failed. A rate limit holds polling back for as
    /// long as GitHub asked.
    pub fn record_failure(&mut self, now: Instant, error: &GitHubError) {
        if let GitHubError::RateLimited { retry_after } = error {
            self.hold_until = Some(now + *retry_after);
        }
    }

    /// When the next poll may start, given that the last one finished at
    /// `now`.
    pub fn next_poll(&mut self, now: Instant, live: bool) -> Instant {
        while self
            .spent
            .front()
            .is_some_and(|&(at, _)| now.duration_since(at) >= HOUR)
        {
            self.spent.pop_front();
        }

        let interval = if live {
            LIVE_INTERVAL
        } else {
            SETTLED_INTERVAL
        };
        let mut next = now + interval;
        if let Some(hold) = self.hold_until {
            next = next.max(hold);
        }

        // Assume the next poll costs what the last one did, and wait until
        // enough of the last hour's polls age out to fit it in the share.
        let allowance = (f64::from(self.limit.unwrap_or(DEFAULT_LIMIT)) * SHARE) as u32;
        let expected = self.spent.back().map_or(0, |&(_, cost)| cost);
        let mut total: u32 = self.spent.iter().map(|&(_, cost)| cost).sum::<u32>() + expected;
        for &(at, cost) in &self.spent {
            if total <= allowance {
                break;
            }
            total -= cost;
            next = next.max(at + HOUR);
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(cost: u32, remaining: u32) -> RateLimit {
        RateLimit {
            cost,
            limit: 5000,
            remaining,
            resets_in: Duration::from_secs(1800),
        }
    }

    /// Polls for an hour of simulated time, each poll costing `cost`, and
    /// returns how many polls ran and the points they spent.
    fn simulate_hour(cost: u32, live: bool) -> (u32, u32) {
        let start = Instant::now();
        let mut pace = Pace::default();
        let mut now = start;
        let (mut polls, mut spent) = (0, 0);
        while now < start + HOUR {
            polls += 1;
            spent += cost;
            pace.record(now, rate(cost, 4000));
            now = pace.next_poll(now, live);
        }
        (polls, spent)
    }

    #[test]
    fn a_cheap_poll_runs_every_30_s_while_live_and_every_2_min_when_settled() {
        assert_eq!(simulate_hour(1, true), (120, 120));
        assert_eq!(simulate_hour(1, false), (30, 30));
    }

    #[test]
    fn expensive_polls_slow_down_to_a_quarter_of_the_hourly_budget() {
        for cost in [11, 25, 100, 400] {
            let (polls, spent) = simulate_hour(cost, true);

            assert!(spent <= 1250, "cost {cost}: spent {spent} in {polls} polls");
            assert!(spent > 1250 - cost, "cost {cost}: only spent {spent}");
        }
    }

    #[test]
    fn a_nearly_spent_budget_waits_for_the_reset() {
        let now = Instant::now();
        let mut pace = Pace::default();

        pace.record(now, rate(1, 100));

        assert_eq!(pace.next_poll(now, true), now + Duration::from_secs(1800));
    }

    #[test]
    fn a_rate_limited_poll_waits_as_long_as_github_asked() {
        let now = Instant::now();
        let mut pace = Pace::default();

        pace.record_failure(
            now,
            &GitHubError::RateLimited {
                retry_after: Duration::from_secs(300),
            },
        );

        assert_eq!(pace.next_poll(now, true), now + Duration::from_secs(300));
    }

    #[test]
    fn any_other_failure_retries_on_the_usual_interval() {
        let now = Instant::now();
        let mut pace = Pace::default();

        pace.record_failure(now, &GitHubError::Other("offline".into()));

        assert_eq!(pace.next_poll(now, false), now + SETTLED_INTERVAL);
    }
}
