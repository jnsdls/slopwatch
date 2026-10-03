//! Budgets: caps, in list-price US dollars, on what Steps may spend per
//! Step, per Watched PR since its last outside push, and per day. The
//! Pipeline sets the first two. The daily Budget is a daemon setting the
//! developer keeps, never the repo's.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An amount in US cents, as Budgets cross the wire. Whole cents keep the
/// commands and entries that carry them comparable.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Cents(pub u64);

impl Cents {
    /// `usd` rounded up to the cent, so a spend never reads as less than
    /// it was. Nothing below zero.
    pub fn from_usd(usd: f64) -> Cents {
        if !usd.is_finite() || usd <= 0.0 {
            return Cents(0);
        }
        // Rounding first keeps $10.00 from reading as 1001 cents.
        Cents(((usd * 100.0 * 1e6).round() / 1e6).ceil() as u64)
    }

    /// The amount in US dollars.
    pub fn usd(self) -> f64 {
        self.0 as f64 / 100.0
    }
}

impl fmt::Display for Cents {
    /// `$10`, or `$10.40` when there are cents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 % 100 {
            0 => write!(f, "${}", self.0 / 100),
            cents => write!(f, "${}.{cents:02}", self.0 / 100),
        }
    }
}

/// The daily Budget when the developer hasn't set one: $25.
pub const DAILY_BUDGET_DEFAULT: Cents = Cents(2500);

/// The developer's settings in the daemon that belong to no one Plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSettings {
    /// What Steps may spend across every repo from local midnight on.
    /// `None` turns the daily Budget off.
    pub daily_budget: Option<Cents>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            daily_budget: Some(DAILY_BUDGET_DEFAULT),
        }
    }
}

/// Which Budget ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    /// The Watched PR's, since its last outside push.
    Pr,
    /// The day's, across every repo.
    Daily,
}

/// A spent Budget, as the Inbox entry it raised shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetHit {
    pub kind: BudgetKind,
    pub spent: Cents,
    pub budget: Cents,
}

impl BudgetHit {
    /// What "raise the Budget" offers: twice the Budget, or the spend plus
    /// the Budget once the spend has gone past it, in whole dollars.
    pub fn raise_to(&self) -> Cents {
        let to = self.spent.0.max(self.budget.0) + self.budget.0.max(100);
        Cents(to.div_ceil(100) * 100)
    }
}

impl fmt::Display for BudgetHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whose = match self.kind {
            BudgetKind::Pr => "the PR's Budget since its last outside push",
            BudgetKind::Daily => "today's Budget",
        };
        write!(f, "Spent {} of {}, {whose}", self.spent, self.budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cents_round_up_and_read_as_dollars() {
        assert_eq!(Cents::from_usd(10.0), Cents(1000));
        assert_eq!(Cents::from_usd(0.0013), Cents(1));
        assert_eq!(Cents::from_usd(-1.0), Cents(0));
        assert_eq!(Cents::from_usd(f64::NAN), Cents(0));
        assert_eq!(Cents(1000).to_string(), "$10");
        assert_eq!(Cents(1040).to_string(), "$10.40");
        assert_eq!(Cents(5).to_string(), "$0.05");
    }

    #[test]
    fn a_raise_doubles_the_budget_or_clears_the_spend_by_a_budget() {
        let hit = |spent, budget| BudgetHit {
            kind: BudgetKind::Pr,
            spent: Cents(spent),
            budget: Cents(budget),
        };
        assert_eq!(hit(1000, 1000).raise_to(), Cents(2000));
        assert_eq!(hit(1450, 1000).raise_to(), Cents(2500));
        assert_eq!(hit(5, 5).raise_to(), Cents(200));
    }

    #[test]
    fn settings_default_to_a_25_dollar_day_and_none_turns_it_off() {
        assert_eq!(
            serde_json::to_value(DaemonSettings::default()).unwrap(),
            json!({ "daily_budget": 2500 })
        );
        let off: DaemonSettings = serde_json::from_value(json!({ "daily_budget": null })).unwrap();
        assert_eq!(off.daily_budget, None);
        assert_eq!(
            BudgetHit {
                kind: BudgetKind::Daily,
                spent: Cents(2510),
                budget: Cents(2500),
            }
            .to_string(),
            "Spent $25.10 of $25, today's Budget"
        );
    }
}
