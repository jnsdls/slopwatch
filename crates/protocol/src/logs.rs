//! Step logs on the wire. A Step log is what one attempt of a Step wrote
//! while it ran: its stderr lines and its `log` messages, one ordered
//! stream of records. Logs travel over the protocol, never as paths
//! (ADR 0010). A client follows a running attempt on its
//! `log/<run>/<step>/<attempt>` topic and pages through the rest, searched
//! and filtered by the daemon, with [`crate::Command::ReadStepLog`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::runs::RunId;

/// One attempt's Step log.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LogKey {
    pub run: RunId,
    pub step: String,
    /// Counts from 1. A respawn after a daemon restart and a retry each
    /// start a new attempt with its own log. `read_step_log` reads 0 as the
    /// Step's latest attempt, for a Step whose Outcome a later Run reused.
    pub attempt: u32,
}

impl fmt::Display for LogKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.run, self.step, self.attempt)
    }
}

impl LogKey {
    /// Reads `<run>/<step>/<attempt>`. The step sits between the first and
    /// the last slash, so it may hold slashes of its own.
    pub fn parse(text: &str) -> Option<Self> {
        let (run, rest) = text.split_once('/')?;
        let (step, attempt) = rest.rsplit_once('/')?;
        if step.is_empty() {
            return None;
        }
        Some(Self {
            run: RunId(run.parse().ok()?),
            step: step.to_owned(),
            attempt: attempt.parse().ok()?,
        })
    }
}

/// One line of a Step log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// The record's place in the attempt's log, counting from 1. Records
    /// the 16 MB cap dropped leave a gap.
    pub seq: u64,
    /// When the daemon read it, in milliseconds since the Unix epoch.
    pub ts: i64,
    pub source: LogSource,
    /// `log` messages may carry a level. Stderr lines never do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    /// A line the Step wrote to stderr.
    Stderr,
    /// A `log` message the Step sent over the Step protocol.
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        })
    }
}

/// Which records a log page holds. The empty filter holds them all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFilter {
    /// Text the record must contain, ignoring case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// The sources to keep. Empty keeps every source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<LogSource>,
    /// The levels to keep. Empty keeps every level. A record without a
    /// level, such as a stderr line, is never dropped for its level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<LogLevel>,
}

impl LogFilter {
    pub fn is_empty(&self) -> bool {
        self.search.as_deref().is_none_or(str::is_empty)
            && self.sources.is_empty()
            && self.levels.is_empty()
    }

    pub fn matches(&self, record: &LogRecord) -> bool {
        if !self.sources.is_empty() && !self.sources.contains(&record.source) {
            return false;
        }
        if let Some(level) = record.level
            && !self.levels.is_empty()
            && !self.levels.contains(&level)
        {
            return false;
        }
        match self.search.as_deref() {
            None | Some("") => true,
            Some(search) => contains_ignoring_case(&record.text, search),
        }
    }
}

fn contains_ignoring_case(text: &str, search: &str) -> bool {
    if text.is_ascii() && search.is_ascii() {
        let (text, search) = (text.as_bytes(), search.as_bytes());
        return text
            .windows(search.len())
            .any(|window| window.eq_ignore_ascii_case(search));
    }
    text.to_lowercase().contains(&search.to_lowercase())
}

/// Which page of matching records to read. Without `before` or `after`,
/// the last page. With both, `after` wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPage {
    /// The matching records just before this sequence number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// The matching records just after this sequence number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// At most this many records. The daemon caps it at
    /// [`MAX_PAGE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// The most records one page holds.
pub const MAX_PAGE: u32 = 1000;

/// One page of a Step log, oldest record first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLogPage {
    pub key: LogKey,
    /// What the page was asked for, so a client can tell its answers apart.
    pub page: LogPage,
    pub filter: LogFilter,
    pub records: Vec<LogRecord>,
    /// Matching records exist before the first one here.
    pub more_before: bool,
    /// Matching records exist after the last one here.
    pub more_after: bool,
    /// Where the 16 MB cap cut the log, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncation>,
}

/// The middle of a log the cap dropped: it keeps the first 1 MB and a
/// rolling last 15 MB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    /// The last record before the gap.
    pub after_seq: u64,
    /// How much log text was dropped, a newline per record included.
    pub bytes: u64,
    pub records: u64,
}

impl fmt::Display for Truncation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes truncated ({} lines)", self.bytes, self.records)
    }
}

/// The "storage over cap" daemon warning: the detail no rule may prune
/// still takes more room than the cap allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageWarning {
    pub used_bytes: u64,
    pub cap_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(source: LogSource, level: Option<LogLevel>, text: &str) -> LogRecord {
        LogRecord {
            seq: 1,
            ts: 0,
            source,
            level,
            text: text.into(),
        }
    }

    #[test]
    fn a_log_key_reads_back_with_slashes_in_the_step() {
        let key = LogKey {
            run: RunId(4),
            step: "a/b".into(),
            attempt: 2,
        };

        assert_eq!(key.to_string(), "4/a/b/2");
        assert_eq!(LogKey::parse("4/a/b/2"), Some(key));
        assert_eq!(LogKey::parse("4//2"), None);
        assert_eq!(LogKey::parse("x/ci/1"), None);
    }

    #[test]
    fn a_record_names_its_source_and_only_log_messages_have_a_level() {
        assert_eq!(
            serde_json::to_value(record(LogSource::Stderr, None, "hi")).unwrap(),
            json!({ "seq": 1, "ts": 0, "source": "stderr", "text": "hi" })
        );
        assert_eq!(
            serde_json::to_value(record(LogSource::Log, Some(LogLevel::Warn), "hi")).unwrap(),
            json!({ "seq": 1, "ts": 0, "source": "log", "level": "warn", "text": "hi" })
        );
    }

    #[test]
    fn the_filter_searches_ignoring_case_and_keeps_levelless_lines() {
        let filter = LogFilter {
            search: Some("TIMEOUT".into()),
            sources: vec![],
            levels: vec![LogLevel::Error],
        };

        assert!(filter.matches(&record(LogSource::Stderr, None, "a timeout here")));
        assert!(filter.matches(&record(LogSource::Log, Some(LogLevel::Error), "Timeout!")));
        assert!(!filter.matches(&record(LogSource::Log, Some(LogLevel::Info), "timeout")));
        assert!(!filter.matches(&record(LogSource::Stderr, None, "all good")));
        assert!(
            LogFilter {
                search: Some("ÉTÉ".into()),
                ..LogFilter::default()
            }
            .matches(&record(LogSource::Stderr, None, "un été chaud"))
        );

        let stderr_only = LogFilter {
            sources: vec![LogSource::Stderr],
            ..LogFilter::default()
        };
        assert!(!stderr_only.matches(&record(LogSource::Log, None, "x")));
        assert!(LogFilter::default().is_empty());
        assert!(!stderr_only.is_empty());
    }
}
