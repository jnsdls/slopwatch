//! What the PR pane knows about Step logs: the last lines of an open Step
//! row, and the full-log viewer. No GPUI here, so it tests without a
//! window.
//!
//! The daemon searches and filters, and the viewer asks it for one page at
//! a time, so a log of any size stays a page in the pane. While the viewer
//! shows the end of the latest attempt it follows the live records too.
//! The Events toggle interleaves the Step's journal events by timestamp.

use std::collections::VecDeque;

use slopwatch_protocol::step::{Effect, EffectResult, UpdateMethod};
use slopwatch_protocol::{
    Command, LogFilter, LogKey, LogLevel, LogPage, LogRecord, LogSource, RunEvent, StepLogPage,
    Truncation,
};

/// How many lines an open Step row shows.
pub const TAIL_LINES: usize = 20;

/// How many records one viewer page asks for.
pub const PAGE: u32 = 500;

/// The most records the viewer holds while it follows a live log. Older
/// ones page back in on request.
const FOLLOW_WINDOW: usize = 2000;

/// The last lines of one attempt's log, as they arrive.
#[derive(Debug, Clone, PartialEq)]
pub struct LogTail {
    pub key: LogKey,
    lines: VecDeque<LogRecord>,
}

impl LogTail {
    pub fn new(key: LogKey) -> Self {
        Self {
            key,
            lines: VecDeque::new(),
        }
    }

    /// The command that follows the log, from the last line the tail has.
    pub fn subscribe(&self) -> Command {
        Command::Subscribe {
            topic: slopwatch_protocol::Topic::StepLog(self.key.clone()),
            since: self.lines.back().map(|record| record.seq),
        }
    }

    pub fn push(&mut self, records: &[LogRecord]) {
        for record in records {
            if self.lines.back().is_some_and(|last| last.seq >= record.seq) {
                continue;
            }
            self.lines.push_back(record.clone());
            if self.lines.len() > TAIL_LINES {
                self.lines.pop_front();
            }
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &LogRecord> {
        self.lines.iter()
    }
}

/// One Run event with when the daemon journalled it.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedEvent {
    pub ts: i64,
    pub event: RunEvent,
}

/// One row of the viewer.
#[derive(Debug, Clone, PartialEq)]
pub enum Row<'a> {
    Line(&'a LogRecord),
    /// Where the 16 MB cap cut the log.
    Truncated(Truncation),
    /// A journal event, interleaved by its timestamp.
    Event {
        ts: i64,
        text: String,
    },
}

/// The full-log viewer on one attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct LogViewer {
    pub key: LogKey,
    pub filter: LogFilter,
    /// Interleave the Step's journal events.
    pub events: bool,
    records: Vec<LogRecord>,
    truncated: Option<Truncation>,
    more_before: bool,
    more_after: bool,
    /// The page asked for last. Answers to anything else are stale.
    asked: Option<LogPage>,
    /// Live records that arrived while the end of the log was loading.
    /// The page may or may not have them.
    early: Vec<LogRecord>,
    /// The last live record heard, matching or not. A record further on
    /// means the daemon skipped some, after the client fell behind.
    live_seq: Option<u64>,
}

impl LogViewer {
    /// Opens the viewer on the end of `key`'s log.
    pub fn open(key: LogKey) -> (Self, Command) {
        let mut viewer = Self {
            key,
            filter: LogFilter::default(),
            events: false,
            records: Vec::new(),
            truncated: None,
            more_before: false,
            more_after: false,
            asked: None,
            early: Vec::new(),
            live_seq: None,
        };
        let command = viewer.ask(LogPage::default());
        (viewer, command)
    }

    fn ask(&mut self, page: LogPage) -> Command {
        let page = LogPage {
            limit: Some(PAGE),
            ..page
        };
        self.asked = Some(page);
        self.early.clear();
        self.live_seq = None;
        Command::ReadStepLog {
            key: self.key.clone(),
            page,
            filter: self.filter.clone(),
        }
    }

    /// Takes a page the daemon sent. Pages for another log, filter or
    /// request are dropped.
    pub fn loaded(&mut self, page: StepLogPage) {
        if page.key != self.key || page.filter != self.filter || Some(page.page) != self.asked {
            return;
        }
        self.asked = None;
        self.records = page.records;
        self.truncated = page.truncated;
        self.more_before = page.more_before;
        self.more_after = page.more_after;
        let early = std::mem::take(&mut self.early);
        let key = self.key.clone();
        // The page was read after these were written, so none was skipped.
        let _ = self.live(&key, &early);
    }

    pub fn loading(&self) -> bool {
        self.asked.is_some()
    }

    /// Whether the viewer shows the end of the log and takes live records.
    pub fn following(&self) -> bool {
        !self.more_after && !self.loading()
    }

    pub fn has_older(&self) -> bool {
        self.more_before
    }

    pub fn has_newer(&self) -> bool {
        self.more_after
    }

    /// The page before the one shown.
    pub fn older(&mut self) -> Option<Command> {
        let first = self.records.first()?.seq;
        self.more_before.then(|| {
            self.ask(LogPage {
                before: Some(first),
                ..LogPage::default()
            })
        })
    }

    /// The page after the one shown.
    pub fn newer(&mut self) -> Option<Command> {
        let last = self.records.last()?.seq;
        self.more_after.then(|| {
            self.ask(LogPage {
                after: Some(last),
                ..LogPage::default()
            })
        })
    }

    /// Back to the end of the log, following it.
    pub fn follow(&mut self) -> Command {
        self.ask(LogPage::default())
    }

    /// Searches and filters from the end of the log.
    pub fn set_filter(&mut self, filter: LogFilter) -> Command {
        self.filter = filter;
        self.ask(LogPage::default())
    }

    pub fn set_search(&mut self, search: &str) -> Command {
        let search = search.trim();
        let filter = LogFilter {
            search: (!search.is_empty()).then(|| search.to_owned()),
            ..self.filter.clone()
        };
        self.set_filter(filter)
    }

    /// Shows or hides one source. Showing every source clears the filter.
    pub fn toggle_source(&mut self, source: LogSource) -> Command {
        let filter = LogFilter {
            sources: toggled(
                &self.filter.sources,
                source,
                &[LogSource::Stderr, LogSource::Log],
            ),
            ..self.filter.clone()
        };
        self.set_filter(filter)
    }

    pub fn toggle_level(&mut self, level: LogLevel) -> Command {
        let all = [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ];
        let filter = LogFilter {
            levels: toggled(&self.filter.levels, level, &all),
            ..self.filter.clone()
        };
        self.set_filter(filter)
    }

    /// Takes records just written to the log, if the viewer follows it.
    /// When the daemon skipped some, it reloads the end of the log instead
    /// and returns the command for that.
    pub fn live(&mut self, key: &LogKey, records: &[LogRecord]) -> Option<Command> {
        if *key != self.key {
            return None;
        }
        let loading_the_end = self
            .asked
            .is_some_and(|page| page.before.is_none() && page.after.is_none());
        if loading_the_end {
            self.early.extend_from_slice(records);
            return None;
        }
        let skipped = match (self.live_seq, records.first()) {
            (Some(heard), Some(next)) => next.seq > heard + 1,
            _ => false,
        };
        if let Some(last) = records.last() {
            self.live_seq = Some(self.live_seq.map_or(last.seq, |heard| heard.max(last.seq)));
        }
        if !self.following() {
            return None;
        }
        if skipped {
            return Some(self.follow());
        }
        let last = self.records.last().map_or(0, |record| record.seq);
        self.records.extend(
            records
                .iter()
                .filter(|record| record.seq > last && self.filter.matches(record))
                .cloned(),
        );
        if self.records.len() > FOLLOW_WINDOW {
            let extra = self.records.len() - FOLLOW_WINDOW;
            self.records.drain(..extra);
            self.more_before = true;
        }
        None
    }

    /// The rows to show: the records, the truncation marker where it
    /// falls, and with the Events toggle on, the Step's journal events
    /// whose time falls within the page.
    pub fn rows<'a>(&'a self, events: &[TimedEvent]) -> Vec<Row<'a>> {
        let mut rows = Vec::new();
        for record in &self.records {
            if let Some(truncated) = self.truncated
                && record.seq > truncated.after_seq
                && rows.last().is_some_and(
                    |row| matches!(row, Row::Line(last) if last.seq <= truncated.after_seq),
                )
            {
                rows.push(Row::Truncated(truncated));
            }
            rows.push(Row::Line(record));
        }
        if !self.events {
            return rows;
        }
        let from = match (self.more_before, self.records.first()) {
            (true, Some(first)) => first.ts,
            _ => i64::MIN,
        };
        let to = match (self.more_after, self.records.last()) {
            (true, Some(last)) => last.ts,
            _ => i64::MAX,
        };
        let mut timed: Vec<(i64, String)> = events
            .iter()
            .filter(|event| (from..=to).contains(&event.ts))
            .filter_map(|event| Some((event.ts, event_text(&event.event, &self.key)?)))
            .collect();
        timed.sort_by_key(|(ts, _)| *ts);
        let mut merged = Vec::with_capacity(rows.len() + timed.len());
        let mut timed = timed.into_iter().peekable();
        for row in rows {
            if let Row::Line(record) = row {
                while let Some((ts, text)) = timed.next_if(|(ts, _)| *ts < record.ts) {
                    merged.push(Row::Event { ts, text });
                }
            }
            merged.push(row);
        }
        merged.extend(timed.map(|(ts, text)| Row::Event { ts, text }));
        merged
    }

    /// The rows as text, for the clipboard.
    pub fn text(&self, events: &[TimedEvent]) -> String {
        let mut text = String::new();
        for row in self.rows(events) {
            text.push_str(&row_text(&row));
            text.push('\n');
        }
        text
    }
}

fn toggled<T: Copy + PartialEq>(on: &[T], value: T, all: &[T]) -> Vec<T> {
    // An empty list means every value is shown.
    let mut shown: Vec<T> = if on.is_empty() {
        all.to_vec()
    } else {
        on.to_vec()
    };
    match shown.iter().position(|&shown| shown == value) {
        Some(at) => {
            shown.remove(at);
        }
        None => shown.push(value),
    }
    if shown.len() == all.len() {
        Vec::new()
    } else {
        shown
    }
}

/// Whether the viewer's filter shows `value`.
pub fn shows<T: PartialEq>(on: &[T], value: &T) -> bool {
    on.is_empty() || on.contains(value)
}

/// How one row reads.
pub fn row_text(row: &Row) -> String {
    match row {
        Row::Line(record) => line_text(record),
        Row::Truncated(truncated) => format!("… {truncated} …"),
        Row::Event { text, .. } => format!("— {text}"),
    }
}

/// How one log line reads: its source or level, then its text.
pub fn line_text(record: &LogRecord) -> String {
    match (record.source, record.level) {
        (_, Some(level)) => format!("[{level}] {}", record.text),
        (LogSource::Log, None) => format!("[log] {}", record.text),
        (LogSource::Stderr, None) => record.text.clone(),
    }
}

/// What a journal event says in the log viewer, if it's about the Step.
fn event_text(event: &RunEvent, key: &LogKey) -> Option<String> {
    match event {
        RunEvent::StepStarted { step, attempt } if *step == key.step => {
            Some(format!("attempt {attempt} started"))
        }
        RunEvent::StepProgress { step, message } if *step == key.step => {
            Some(format!("progress: {message}"))
        }
        RunEvent::StepSettled {
            step,
            verdict,
            reason,
            ..
        } if *step == key.step => Some(match reason {
            Some(reason) => format!("Verdict {verdict}: {reason}"),
            None => format!("Verdict {verdict}"),
        }),
        RunEvent::Effect {
            step,
            effect,
            result,
        } if *step == key.step => {
            let effect = match effect {
                Effect::Comment { .. } => "comment".to_owned(),
                Effect::Label {
                    name,
                    remove: false,
                } => format!("label `{name}`"),
                Effect::Label { name, remove: true } => format!("removing label `{name}`"),
                Effect::Rerun { check, .. } => format!("rerun of `{check}`"),
                Effect::Rebase {
                    method: UpdateMethod::Rebase,
                } => "rebase onto the base".to_owned(),
                Effect::Rebase {
                    method: UpdateMethod::Merge,
                } => "merge of the base into the branch".to_owned(),
                Effect::Merge { .. } => "merge".to_owned(),
            };
            Some(match result {
                EffectResult::Done => format!("{effect}: done"),
                EffectResult::Enqueued => format!("{effect}: in the merge queue"),
                EffectResult::Dropped { reason } => format!("{effect}: dropped, {reason}"),
                EffectResult::Refused { reason } => format!("{effect}: refused, {reason}"),
                EffectResult::Failed { reason } => format!("{effect}: failed, {reason}"),
            })
        }
        _ => None,
    }
}

/// A Unix time in seconds as a UTC date, `2026-10-02`.
pub fn date(unix_secs: i64) -> String {
    // Howard Hinnant's days-to-civil.
    let days = unix_secs.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_core::Verdict;
    use slopwatch_protocol::RunId;

    fn key() -> LogKey {
        LogKey {
            run: RunId(1),
            step: "ci".into(),
            attempt: 1,
        }
    }

    fn record(seq: u64, ts: i64, text: &str) -> LogRecord {
        LogRecord {
            seq,
            ts,
            source: LogSource::Stderr,
            level: None,
            text: text.into(),
        }
    }

    fn page_of(
        viewer: &LogViewer,
        records: Vec<LogRecord>,
        before: bool,
        after: bool,
    ) -> StepLogPage {
        StepLogPage {
            key: viewer.key.clone(),
            page: viewer.asked.expect("a page was asked for"),
            filter: viewer.filter.clone(),
            records,
            more_before: before,
            more_after: after,
            truncated: None,
        }
    }

    fn lines(viewer: &LogViewer) -> Vec<String> {
        viewer.rows(&[]).iter().map(row_text).collect()
    }

    #[test]
    fn the_tail_keeps_the_last_20_lines_once_each() {
        let mut tail = LogTail::new(key());
        let records: Vec<_> = (1..=30)
            .map(|n| record(n, n as i64, &n.to_string()))
            .collect();

        tail.push(&records[..25]);
        tail.push(&records[20..]);

        let shown: Vec<u64> = tail.lines().map(|record| record.seq).collect();
        assert_eq!(shown, (11..=30).collect::<Vec<_>>());
        assert_eq!(
            tail.subscribe(),
            Command::Subscribe {
                topic: slopwatch_protocol::Topic::StepLog(key()),
                since: Some(30),
            }
        );
    }

    #[test]
    fn the_viewer_pages_back_then_follows_again() {
        let (mut viewer, open) = LogViewer::open(key());
        assert!(
            matches!(open, Command::ReadStepLog { page, .. } if page.before.is_none() && page.after.is_none())
        );
        let last = page_of(
            &viewer,
            vec![record(9, 9, "nine"), record(10, 10, "ten")],
            true,
            false,
        );
        viewer.loaded(last);
        assert!(viewer.following());

        let Some(Command::ReadStepLog { page, .. }) = viewer.older() else {
            panic!("there's an older page");
        };
        assert_eq!(page.before, Some(9));
        assert!(!viewer.following(), "nothing live while a page loads");
        viewer.loaded(page_of(&viewer, vec![record(8, 8, "eight")], false, true));
        assert_eq!(lines(&viewer), ["eight"]);
        assert!(viewer.older().is_none(), "the first page");

        viewer.live(&key(), &[record(11, 11, "eleven")]);
        assert_eq!(lines(&viewer), ["eight"], "an older page doesn't follow");

        let Some(Command::ReadStepLog { page, .. }) = viewer.newer() else {
            panic!("there's a newer page");
        };
        assert_eq!(page.after, Some(8));
        viewer.loaded(page_of(
            &viewer,
            vec![
                record(9, 9, "nine"),
                record(10, 10, "ten"),
                record(11, 11, "eleven"),
            ],
            true,
            false,
        ));
        viewer.live(
            &key(),
            &[record(11, 11, "eleven"), record(12, 12, "twelve")],
        );
        assert_eq!(lines(&viewer), ["nine", "ten", "eleven", "twelve"]);
    }

    #[test]
    fn records_that_arrive_while_the_end_loads_land_after_it() {
        let (mut viewer, _) = LogViewer::open(key());

        viewer.live(&key(), &[record(2, 2, "two"), record(3, 3, "three")]);
        let page = page_of(
            &viewer,
            vec![record(1, 1, "one"), record(2, 2, "two")],
            false,
            false,
        );
        viewer.loaded(page);

        assert_eq!(lines(&viewer), ["one", "two", "three"]);
    }

    #[test]
    fn records_the_daemon_skipped_reload_the_end() {
        let (mut viewer, _) = LogViewer::open(key());
        viewer.loaded(page_of(&viewer, vec![record(1, 1, "one")], false, false));
        assert_eq!(viewer.live(&key(), &[record(2, 2, "two")]), None);

        let reload = viewer.live(&key(), &[record(500, 500, "far on")]);

        assert!(matches!(
            reload,
            Some(Command::ReadStepLog { page, .. }) if page.before.is_none() && page.after.is_none()
        ));
        assert!(viewer.loading());
    }

    #[test]
    fn a_stale_page_is_dropped() {
        let (mut viewer, _) = LogViewer::open(key());
        let stale = page_of(&viewer, vec![record(1, 1, "old")], false, false);
        let _ = viewer.set_search("fail");

        viewer.loaded(stale);

        assert!(lines(&viewer).is_empty());
        assert!(viewer.loading());
    }

    #[test]
    fn search_and_filters_ask_the_daemon_and_apply_to_live_records() {
        let (mut viewer, _) = LogViewer::open(key());
        let Command::ReadStepLog { filter, page, .. } = viewer.set_search("  Fail ") else {
            panic!("a search reads a page");
        };
        assert_eq!(filter.search.as_deref(), Some("Fail"));
        assert_eq!(page.before, None);
        viewer.loaded(page_of(&viewer, vec![], false, false));

        viewer.live(&key(), &[record(1, 1, "ok"), record(2, 2, "it failed")]);
        assert_eq!(lines(&viewer), ["it failed"]);

        let Command::ReadStepLog { filter, .. } = viewer.toggle_source(LogSource::Log) else {
            panic!();
        };
        assert_eq!(
            filter.sources,
            [LogSource::Stderr],
            "hiding log keeps stderr"
        );
        let Command::ReadStepLog { filter, .. } = viewer.toggle_source(LogSource::Log) else {
            panic!();
        };
        assert!(filter.sources.is_empty(), "both shown means no filter");
        let Command::ReadStepLog { filter, .. } = viewer.toggle_level(LogLevel::Debug) else {
            panic!();
        };
        assert_eq!(
            filter.levels,
            [LogLevel::Info, LogLevel::Warn, LogLevel::Error]
        );
    }

    #[test]
    fn the_truncation_marker_sits_in_the_gap() {
        let (mut viewer, _) = LogViewer::open(key());
        let mut page = page_of(
            &viewer,
            vec![record(1, 1, "head"), record(50, 2, "tail")],
            false,
            false,
        );
        page.truncated = Some(Truncation {
            after_seq: 1,
            bytes: 2048,
            records: 48,
        });
        viewer.loaded(page);

        assert_eq!(
            lines(&viewer),
            ["head", "… 2048 bytes truncated (48 lines) …", "tail"]
        );
    }

    #[test]
    fn events_interleave_by_time_within_the_page() {
        let (mut viewer, _) = LogViewer::open(key());
        viewer.loaded(page_of(
            &viewer,
            vec![record(5, 100, "first"), record(6, 300, "second")],
            true,
            false,
        ));
        let events = [
            TimedEvent {
                ts: 50,
                event: RunEvent::StepProgress {
                    step: "ci".into(),
                    message: "before the page".into(),
                },
            },
            TimedEvent {
                ts: 200,
                event: RunEvent::StepProgress {
                    step: "ci".into(),
                    message: "halfway".into(),
                },
            },
            TimedEvent {
                ts: 250,
                event: RunEvent::StepProgress {
                    step: "other".into(),
                    message: "another Step".into(),
                },
            },
            TimedEvent {
                ts: 400,
                event: RunEvent::StepSettled {
                    step: "ci".into(),
                    verdict: Verdict::Pass,
                    reason: None,
                    outputs: Default::default(),
                    reused_from: None,
                },
            },
        ];

        assert_eq!(lines(&viewer), ["first", "second"], "off by default");
        viewer.events = true;
        let rows: Vec<String> = viewer.rows(&events).iter().map(row_text).collect();
        assert_eq!(
            rows,
            ["first", "— progress: halfway", "second", "— Verdict pass"]
        );
    }

    #[test]
    fn an_effect_reads_as_what_it_asked_and_what_became_of_it() {
        let effect = |effect, result| RunEvent::Effect {
            step: "ci".into(),
            effect,
            result,
        };
        let rerun = Effect::Rerun {
            check: "test".into(),
            job: 7,
        };

        assert_eq!(
            event_text(&effect(rerun.clone(), EffectResult::Done), &key()).as_deref(),
            Some("rerun of `test`: done")
        );
        assert_eq!(
            event_text(
                &effect(
                    Effect::Comment { body: "hi".into() },
                    EffectResult::Dropped {
                        reason: "the Run had ended".into()
                    }
                ),
                &key()
            )
            .as_deref(),
            Some("comment: dropped, the Run had ended")
        );
    }

    #[test]
    fn dates_read_as_utc_days() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_790_899_200), "2026-10-02");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}
