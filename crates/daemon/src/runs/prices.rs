//! List prices for Steps that report tokens but no cost, such as Codex
//! (Step contract). A Budget counts list price whatever the developer is
//! billed, so a Step on a subscription login gets priced too.
//!
//! The table holds per-million-token USD prices for the standard tier,
//! from <https://developers.openai.com/api/docs/pricing>, read on
//! 2026-10-03. A model it doesn't name stays unpriced, and its cost reads
//! as unknown, never zero. Claude reports its own cost, so no Anthropic
//! model is here.

use slopwatch_protocol::step::Usage;

/// USD per million tokens.
struct Price {
    input: f64,
    cached_input: f64,
    output: f64,
}

const PRICES: &[(&str, Price)] = &[
    ("gpt-5.6-sol", price(4.00, 0.40, 20.00)),
    ("gpt-5.6-terra", price(2.00, 0.20, 12.00)),
    ("gpt-5.6-luna", price(0.20, 0.02, 1.20)),
    ("gpt-5.5", price(5.00, 0.50, 30.00)),
    ("gpt-5.4", price(2.50, 0.25, 15.00)),
    ("gpt-5.4-mini", price(0.75, 0.075, 4.50)),
    ("gpt-5.3-codex", price(1.75, 0.175, 14.00)),
];

const fn price(input: f64, cached_input: f64, output: f64) -> Price {
    Price {
        input,
        cached_input,
        output,
    }
}

/// What `usage` cost at list price: the Step's own figure if it gave one,
/// else the table's for its model and tokens. `None` when neither knows.
pub fn usd(usage: &Usage) -> Option<f64> {
    if let Some(usd) = usage.usd {
        // A price below zero or not a number would undercount a Budget.
        return (usd.is_finite() && usd >= 0.0).then_some(usd);
    }
    let model = usage.model.as_str();
    let (_, price) = PRICES.iter().find(|(name, _)| *name == model)?;
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let fresh = usage.input_tokens - cached;
    let per_token = |tokens: u64, per_million: f64| tokens as f64 * per_million / 1_000_000.0;
    Some(
        per_token(fresh, price.input)
            + per_token(cached, price.cached_input)
            + per_token(usage.output_tokens, price.output),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(model: &str, input: u64, cached: u64, output: u64) -> Usage {
        Usage {
            model: model.into(),
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            usd: None,
        }
    }

    #[test]
    fn tokens_are_priced_with_cached_input_at_its_own_rate() {
        // 1M fresh input, 1M cached, 1M output on gpt-5.4.
        let cost = usd(&usage("gpt-5.4", 2_000_000, 1_000_000, 1_000_000)).unwrap();
        assert!((cost - (2.50 + 0.25 + 15.00)).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn the_steps_own_figure_wins_and_an_unknown_model_has_no_price() {
        let mut reported = usage("claude-opus-5-5", 10, 0, 10);
        reported.usd = Some(0.42);
        assert_eq!(usd(&reported), Some(0.42));
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            reported.usd = Some(bad);
            assert_eq!(usd(&reported), None, "{bad}");
        }
        assert_eq!(usd(&usage("some-new-model", 10, 0, 10)), None);
        assert_eq!(
            usd(&Usage {
                input_tokens: 10,
                ..Usage::default()
            }),
            None
        );
    }
}
