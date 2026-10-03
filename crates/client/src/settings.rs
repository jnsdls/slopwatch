//! What the daemon settings screen shows: the daily Budget and what Steps
//! spent today, with the command that saves it. No GPUI here, so it tests
//! without a window.

use slopwatch_protocol::{Cents, Command, DaemonSettings};

#[derive(Debug, Default)]
pub struct SettingsModel {
    /// `None` until the daemon answers.
    settings: Option<DaemonSettings>,
    spent_today: Cents,
}

impl SettingsModel {
    pub fn listed(&mut self, settings: DaemonSettings, spent_today: Cents) {
        self.settings = Some(settings);
        self.spent_today = spent_today;
    }

    pub fn loaded(&self) -> bool {
        self.settings.is_some()
    }

    /// The daily Budget as the field shows it: dollars, blank when off.
    pub fn daily_field(&self) -> String {
        match self.settings.and_then(|settings| settings.daily_budget) {
            Some(daily) => daily.to_string().trim_start_matches('$').to_owned(),
            None => String::new(),
        }
    }

    /// What Steps spent today, against the daily Budget if it's on.
    pub fn spent_line(&self) -> String {
        match self.settings.and_then(|settings| settings.daily_budget) {
            Some(daily) => format!("Spent today: {} of {daily}", self.spent_today),
            None => format!(
                "Spent today: {}. The daily Budget is off.",
                self.spent_today
            ),
        }
    }
}

/// The command that saves `field`, the daily Budget in dollars. Blank
/// turns it off.
pub fn save_daily(field: &str) -> Result<Command, String> {
    let field = field.trim().trim_start_matches('$');
    let daily_budget = if field.is_empty() {
        None
    } else {
        let usd: f64 = field
            .parse()
            .ok()
            .filter(|usd: &f64| usd.is_finite() && *usd > 0.0)
            .ok_or_else(|| {
                format!("Write the daily Budget in dollars, such as 25, or leave it blank to turn it off. \"{field}\" isn't one.")
            })?;
        Some(Cents::from_usd(usd))
    };
    Ok(Command::SetSettings {
        settings: DaemonSettings { daily_budget },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_shows_dollars_and_blank_turns_the_budget_off() {
        let mut model = SettingsModel::default();
        assert!(!model.loaded());
        model.listed(DaemonSettings::default(), Cents(120));

        assert_eq!(model.daily_field(), "25");
        assert_eq!(model.spent_line(), "Spent today: $1.20 of $25");
        assert_eq!(
            save_daily(" $12.50 "),
            Ok(Command::SetSettings {
                settings: DaemonSettings {
                    daily_budget: Some(Cents(1250)),
                },
            })
        );
        assert_eq!(
            save_daily(""),
            Ok(Command::SetSettings {
                settings: DaemonSettings { daily_budget: None },
            })
        );
        assert!(save_daily("0").is_err());
        assert!(save_daily("lots").is_err());

        model.listed(DaemonSettings { daily_budget: None }, Cents(0));
        assert_eq!(model.daily_field(), "");
        assert_eq!(
            model.spent_line(),
            "Spent today: $0. The daily Budget is off."
        );
    }
}
