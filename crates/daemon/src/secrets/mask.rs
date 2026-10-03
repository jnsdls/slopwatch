//! Masking: what a Step writes never carries the value of a Secret it
//! received, in its log, its Outcome, its Effects or its errors.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;
use slopwatch_protocol::step::FromStep;

/// What a masked value becomes.
pub const MASKED: &str = "***";

/// The Secret values one Step received, ready to cut out of text.
#[derive(Debug, Clone, Default)]
pub struct Mask {
    /// Longest first, so a value that contains another goes whole.
    patterns: Arc<Vec<String>>,
}

impl Mask {
    /// Masks each value, and its JSON-escaped form where that differs, so
    /// a value echoed inside a broken protocol line goes too.
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        for value in values.into_iter().filter(|value| !value.is_empty()) {
            let escaped = serde_json::to_string(value).expect("strings always serialize");
            let escaped = &escaped[1..escaped.len() - 1];
            for pattern in [value, escaped] {
                if !patterns.iter().any(|known| known == pattern) {
                    patterns.push(pattern.to_owned());
                }
            }
        }
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
        Self {
            patterns: Arc::new(patterns),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The longest pattern, in bytes.
    pub fn longest(&self) -> usize {
        self.patterns.first().map_or(0, String::len)
    }

    /// Where values sit in `text`: byte ranges, in order, not overlapping,
    /// the longest value winning where two start at the same place.
    pub fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        if self.is_empty() || !self.patterns.iter().any(|p| text.contains(p.as_str())) {
            return found;
        }
        let mut at = 0;
        while at < text.len() {
            let rest = &text[at..];
            match self.patterns.iter().find(|p| rest.starts_with(p.as_str())) {
                Some(pattern) => {
                    found.push((at, at + pattern.len()));
                    at += pattern.len();
                }
                None => {
                    at += rest.chars().next().map_or(1, char::len_utf8);
                }
            }
        }
        found
    }

    /// `text` with every value replaced by [`MASKED`].
    pub fn apply<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let found = self.find(text);
        if found.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut masked = String::with_capacity(text.len());
        let mut from = 0;
        for (start, end) in found {
            masked.push_str(&text[from..start]);
            masked.push_str(MASKED);
            from = end;
        }
        masked.push_str(&text[from..]);
        Cow::Owned(masked)
    }

    /// The message with every string in it masked, keys included. `None`
    /// when masking broke the message's shape, as when a value is also a
    /// word of the protocol.
    pub fn message(&self, message: FromStep) -> Option<FromStep> {
        if self.is_empty() {
            return Some(message);
        }
        let mut value = serde_json::to_value(&message).expect("Step messages always serialize");
        if !self.value(&mut value) {
            return Some(message);
        }
        serde_json::from_value(value).ok()
    }

    /// Masks every string in `value`. Returns whether anything changed.
    fn value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) => match self.apply(text) {
                Cow::Borrowed(_) => false,
                Cow::Owned(masked) => {
                    *text = masked;
                    true
                }
            },
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.value(item) | changed),
            Value::Object(map) => {
                let old = std::mem::take(map);
                let mut changed = false;
                for (key, mut item) in old {
                    changed |= self.value(&mut item);
                    let key = match self.apply(&key) {
                        Cow::Borrowed(_) => key,
                        Cow::Owned(masked) => {
                            changed = true;
                            masked
                        }
                    };
                    map.insert(key, item);
                }
                changed
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }
}

/// Masks a stream that arrives in pieces, such as a stderr line too long
/// for one record, where a value can straddle two pieces. It holds back
/// the end of a piece that could be the start of a value until the next
/// piece, or the end of the line, shows whether it is.
#[derive(Debug, Default)]
pub struct StreamMask {
    carry: String,
}

impl StreamMask {
    /// The masked text that's safe to write now. `ended` says the line
    /// ends with this piece, which lets everything held back go.
    pub fn piece(&mut self, mask: &Mask, piece: &str, ended: bool) -> String {
        let mut text = std::mem::take(&mut self.carry);
        text.push_str(piece);
        if ended || mask.is_empty() {
            return mask.apply(&text).into_owned();
        }
        // A value that starts before `cut` is wholly in `text`: it's no
        // longer than `longest`. One that starts after may run on.
        let mut cut = text.len().saturating_sub(mask.longest().saturating_sub(1));
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(&(start, _)) = mask
            .find(&text)
            .iter()
            .find(|&&(start, end)| start < cut && end > cut)
        {
            cut = start;
        }
        self.carry = text.split_off(cut);
        mask.apply(&text).into_owned()
    }

    /// What's held back, masked, for a stream that closed mid-line.
    pub fn finish(&mut self, mask: &Mask) -> Option<String> {
        if self.carry.is_empty() {
            return None;
        }
        let text = std::mem::take(&mut self.carry);
        Some(mask.apply(&text).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::step::{Effect, Finding, Outcome, Severity};

    const KEY: &str = "sk-live-0123456789";

    #[test]
    fn every_occurrence_goes_and_a_longer_value_wins() {
        let mask = Mask::new(["sk-live-01", KEY]);

        assert_eq!(
            mask.apply(&format!("a {KEY} b sk-live-01 {KEY}{KEY}")),
            "a *** b *** ******"
        );
        assert!(matches!(mask.apply("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_value_with_quotes_goes_in_its_json_escaped_form_too() {
        let mask = Mask::new(["pa\"ss\\word-1234"]);

        assert_eq!(
            mask.apply(r#"{"oops": "pa\"ss\\word-1234"} pa"ss\word-1234"#),
            r#"{"oops": "***"} ***"#
        );
    }

    #[test]
    fn an_empty_mask_changes_nothing() {
        let mask = Mask::new([""]);

        assert!(mask.is_empty());
        assert_eq!(mask.apply("anything"), "anything");
    }

    #[test]
    fn a_message_gets_every_string_masked() {
        let mask = Mask::new([KEY]);
        let mut outcome = Outcome::new(slopwatch_core::Verdict::Fail);
        outcome.outputs.findings.push(Finding {
            severity: Severity::Error,
            message: format!("key {KEY} rejected"),
            file: None,
            line: None,
        });
        outcome.outputs.note = Some(KEY.into());

        let Some(FromStep::Outcome(masked)) = mask.message(FromStep::Outcome(outcome)) else {
            panic!("still an outcome");
        };
        assert_eq!(masked.outputs.findings[0].message, "key *** rejected");
        assert_eq!(masked.outputs.note.as_deref(), Some("***"));

        let comment = FromStep::Effect {
            id: "c".into(),
            effect: Effect::Comment {
                body: format!("here: {KEY}"),
            },
        };
        assert_eq!(
            mask.message(comment),
            Some(FromStep::Effect {
                id: "c".into(),
                effect: Effect::Comment {
                    body: "here: ***".into()
                },
            })
        );
    }

    #[test]
    fn a_message_that_masking_breaks_is_none() {
        let mask = Mask::new(["outcome"]);

        assert_eq!(
            mask.message(FromStep::Outcome(Outcome::new(
                slopwatch_core::Verdict::Pass
            ))),
            None
        );
    }

    #[test]
    fn a_value_split_across_pieces_still_goes() {
        let mask = Mask::new([KEY]);
        let line = format!("{}{KEY}{}", "x".repeat(30), "y".repeat(30));
        // Every way of cutting the line in two, and in many pieces.
        for size in 1..line.len() {
            let mut stream = StreamMask::default();
            let mut written = String::new();
            let pieces: Vec<&str> = line
                .as_bytes()
                .chunks(size)
                .map(|chunk| std::str::from_utf8(chunk).unwrap())
                .collect();
            for (index, piece) in pieces.iter().enumerate() {
                let out = stream.piece(&mask, piece, index == pieces.len() - 1);
                assert!(!out.contains("sk-live"), "piece size {size} leaked a part");
                written.push_str(&out);
            }
            assert_eq!(written, format!("{}***{}", "x".repeat(30), "y".repeat(30)));
            assert_eq!(stream.finish(&mask), None);
        }
    }

    #[test]
    fn what_a_closed_stream_held_back_comes_out_masked() {
        let mask = Mask::new([KEY]);
        let mut stream = StreamMask::default();

        let text = "abcdefghijklmnopqrstuvwxyz sk-live-0123";
        let first = stream.piece(&mask, text, false);
        assert_eq!(first, "abcdefghijklmnopqrstuv");
        let rest = stream.finish(&mask).unwrap();
        assert_eq!(format!("{first}{rest}"), text);
        assert_eq!(stream.finish(&mask), None);
    }
}
