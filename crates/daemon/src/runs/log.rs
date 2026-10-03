//! Step logs on disk: one directory per attempt, holding the attempt's
//! records as JSON lines.
//!
//! A log is capped at 16 MB: it keeps the first 1 MB and a rolling last
//! 15 MB. The first 1 MB is the head segment. After it, records go to tail
//! segments of 1 MB each, and once the tail holds more than 15 MB the
//! oldest tail segment is deleted. What was dropped is counted in
//! `truncated.json`, which readers turn into the "N bytes truncated"
//! marker. The Step never notices: it keeps running.
//!
//! Each segment is named after its first record's sequence number, and
//! the records in it are numbered one after another, so a reader finds a
//! record without parsing what's before it.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use slopwatch_protocol::{
    LogFilter, LogKey, LogLevel, LogPage, LogRecord, LogSource, MAX_PAGE, Truncation,
};
use tokio::sync::broadcast;

use super::now_ms;
use crate::secrets::{Mask, StreamMask};

/// Records a slow subscriber may fall behind by. One that falls further
/// reads the rest back from disk.
const BACKLOG: usize = 4096;

const TRUNCATED: &str = "truncated.json";

/// How big a log and its parts may grow.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The head, kept whatever else happens.
    pub head: u64,
    /// The rolling tail after it.
    pub tail: u64,
    /// One tail segment, the unit the tail drops.
    pub segment: u64,
    /// The longest record text. A longer line becomes several records.
    pub line: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            head: 1024 * 1024,
            tail: 15 * 1024 * 1024,
            segment: 1024 * 1024,
            line: 64 * 1024,
        }
    }
}

/// Live delivery of records as they're written, across every log.
#[derive(Clone)]
pub struct Hub {
    sender: broadcast::Sender<(LogKey, LogRecord)>,
}

pub type LiveLog = broadcast::Receiver<(LogKey, LogRecord)>;

impl Default for Hub {
    fn default() -> Self {
        Self {
            sender: broadcast::channel(BACKLOG).0,
        }
    }
}

impl Hub {
    pub fn subscribe(&self) -> LiveLog {
        self.sender.subscribe()
    }
}

struct Segment {
    first_seq: u64,
    /// What the segment takes on disk.
    bytes: u64,
    /// The log text in it, a newline per record, for the truncation count.
    text_bytes: u64,
    records: u64,
}

/// Appends one attempt's records to its directory and sends each to the
/// hub once it's on disk.
pub struct Writer {
    dir: PathBuf,
    key: LogKey,
    limits: Limits,
    hub: Hub,
    next_seq: u64,
    /// The head segment, while it still takes records.
    head: Option<Segment>,
    /// Tail segments, oldest first. The last one takes records.
    tail: VecDeque<Segment>,
    file: Option<File>,
    truncated: Option<Truncation>,
    failed: bool,
    /// The Secret values the Step received, which never reach the log.
    mask: Mask,
    /// The end of a stderr line's last piece, held back while it could be
    /// the start of a value.
    stderr: StreamMask,
}

impl Writer {
    /// Starts the log in `dir`, replacing anything there.
    pub fn create(dir: PathBuf, key: LogKey, limits: Limits, hub: Hub) -> io::Result<Self> {
        match fs::remove_dir_all(&dir) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            key,
            limits,
            hub,
            next_seq: 1,
            head: Some(Segment {
                first_seq: 1,
                bytes: 0,
                text_bytes: 0,
                records: 0,
            }),
            tail: VecDeque::new(),
            file: None,
            truncated: None,
            failed: false,
            mask: Mask::default(),
            stderr: StreamMask::default(),
        })
    }

    /// Masks `mask`'s values out of everything written from now on.
    pub fn with_mask(mut self, mask: Mask) -> Self {
        self.mask = mask;
        self
    }

    /// The longest record text.
    pub fn line_limit(&self) -> usize {
        self.limits.line
    }

    /// Writes `text`, masked and split into records no longer than the
    /// line limit.
    pub fn write(&mut self, source: LogSource, level: Option<LogLevel>, text: &str) {
        let masked = self.mask.apply(text).into_owned();
        self.write_masked(source, level, &masked);
    }

    /// Writes one piece of a stderr line. A line too long for one record
    /// comes in pieces, and `ended` marks its last; a value cut in two by
    /// pieces is still masked.
    pub fn write_stderr(&mut self, piece: &str, ended: bool) {
        let masked = self.stderr.piece(&self.mask, piece, ended);
        if ended || !masked.is_empty() {
            self.write_masked(LogSource::Stderr, None, &masked);
        }
    }

    /// Writes what stderr held back when it closed mid-line.
    pub fn finish_stderr(&mut self) {
        if let Some(masked) = self.stderr.finish(&self.mask) {
            self.write_masked(LogSource::Stderr, None, &masked);
        }
    }

    fn write_masked(&mut self, source: LogSource, level: Option<LogLevel>, text: &str) {
        let mut rest = text;
        loop {
            let at = split_at(rest, self.limits.line);
            let (chunk, after) = rest.split_at(at);
            self.record(LogRecord {
                seq: self.next_seq,
                ts: now_ms(),
                source,
                level,
                text: chunk.to_owned(),
            });
            if after.is_empty() {
                return;
            }
            rest = after;
        }
    }

    fn record(&mut self, record: LogRecord) {
        let mut line = serde_json::to_string(&record).expect("log records always serialize");
        line.push('\n');
        let bytes = line.len() as u64;
        let text_bytes = record.text.len() as u64 + 1;
        if let Err(error) = self.append(&line, bytes, text_bytes) {
            if !self.failed {
                eprintln!(
                    "slopwatchd: can't write the Step log in {}: {error}",
                    self.dir.display()
                );
            }
            self.failed = true;
            return;
        }
        self.next_seq += 1;
        // No subscribers is fine: the record is on disk.
        let _ = self.hub.sender.send((self.key.clone(), record));
    }

    fn append(&mut self, line: &str, bytes: u64, text_bytes: u64) -> io::Result<()> {
        let seq = self.next_seq;
        let head_full = self
            .head
            .as_ref()
            .is_some_and(|head| head.records > 0 && head.bytes + bytes > self.limits.head);
        if head_full {
            self.head = None;
            self.file = None;
        }
        let segment = match &mut self.head {
            Some(head) => head,
            None => {
                let full = self.tail.back().is_none_or(|last| {
                    last.records > 0 && last.bytes + bytes > self.limits.segment
                });
                if full {
                    self.tail.push_back(Segment {
                        first_seq: seq,
                        bytes: 0,
                        text_bytes: 0,
                        records: 0,
                    });
                    self.file = None;
                }
                self.tail.back_mut().expect("just ensured")
            }
        };
        if self.file.is_none() {
            self.file = Some(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(segment_path(&self.dir, segment.first_seq))?,
            );
        }
        self.file
            .as_mut()
            .expect("just opened")
            .write_all(line.as_bytes())?;
        segment.bytes += bytes;
        segment.text_bytes += text_bytes;
        segment.records += 1;
        self.roll()
    }

    /// Drops the oldest tail segments while the tail is over its limit.
    fn roll(&mut self) -> io::Result<()> {
        let mut size: u64 = self.tail.iter().map(|segment| segment.bytes).sum();
        while size > self.limits.tail && self.tail.len() > 1 {
            let dropped = self.tail.pop_front().expect("more than one");
            fs::remove_file(segment_path(&self.dir, dropped.first_seq))?;
            size -= dropped.bytes;
            let truncated = self.truncated.get_or_insert(Truncation {
                after_seq: dropped.first_seq - 1,
                bytes: 0,
                records: 0,
            });
            truncated.bytes += dropped.text_bytes;
            truncated.records += dropped.records;
            let text = serde_json::to_string(truncated).expect("truncations always serialize");
            fs::write(self.dir.join(TRUNCATED), text)?;
        }
        Ok(())
    }
}

/// Where a line splits so its first part fits in `max` bytes, on a char
/// boundary.
fn split_at(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut at = max.max(1);
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    if at == 0 {
        // One char wider than the limit still goes whole.
        at = text.chars().next().map_or(text.len(), char::len_utf8);
    }
    at
}

fn segment_path(dir: &Path, first_seq: u64) -> PathBuf {
    dir.join(format!("{first_seq}.jsonl"))
}

/// One page of the log in `dir`.
#[derive(Debug, Default, PartialEq)]
pub struct Page {
    pub records: Vec<LogRecord>,
    pub more_before: bool,
    pub more_after: bool,
    pub truncated: Option<Truncation>,
}

/// Reads the page `page` of the records in `dir` that `filter` keeps. A
/// directory that doesn't exist reads as an empty log.
pub fn read_page(dir: &Path, page: LogPage, filter: &LogFilter) -> io::Result<Page> {
    let limit = page.limit.unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE) as usize;
    let mut found = Page {
        truncated: truncation(dir)?,
        ..Page::default()
    };
    // Raw lines in the page so far: only these get parsed when nothing
    // needs filtering.
    let mut kept: VecDeque<String> = VecDeque::new();
    for_each_line(dir, |seq, line| {
        let wanted = filter.is_empty() || parse(line).is_some_and(|record| filter.matches(&record));
        if !wanted {
            return true;
        }
        match page.after {
            Some(after) if seq <= after => found.more_before = true,
            Some(_) => {
                if kept.len() == limit {
                    found.more_after = true;
                    return false;
                }
                kept.push_back(line.to_owned());
            }
            None => match page.before {
                Some(before) if seq >= before => {
                    found.more_after = true;
                    return false;
                }
                _ => {
                    if kept.len() == limit {
                        kept.pop_front();
                        found.more_before = true;
                    }
                    kept.push_back(line.to_owned());
                }
            },
        }
        true
    })?;
    found.records = kept.iter().filter_map(|line| parse(line)).collect();
    Ok(found)
}

/// The records after `after`, at most the latest `max` of them.
pub fn read_after(dir: &Path, after: u64, max: u32) -> io::Result<Vec<LogRecord>> {
    let last = read_page(
        dir,
        LogPage {
            limit: Some(max),
            ..LogPage::default()
        },
        &LogFilter::default(),
    )?;
    Ok(last
        .records
        .into_iter()
        .filter(|record| record.seq > after)
        .collect())
}

/// Calls `each` with every record's sequence number and line, in order,
/// until it returns false.
fn for_each_line(dir: &Path, mut each: impl FnMut(u64, &str) -> bool) -> io::Result<()> {
    for (first_seq, path) in segments(dir)? {
        let file = match File::open(&path) {
            Ok(file) => file,
            // The writer rolled it away since the listing.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for (seq, line) in (first_seq..).zip(BufReader::new(file).lines()) {
            let line = line?;
            if !each(seq, &line) {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn segments(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let first_seq = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(|seq| seq.parse::<u64>().ok());
        if let Some(first_seq) = first_seq {
            segments.push((first_seq, path));
        }
    }
    segments.sort();
    Ok(segments)
}

fn truncation(dir: &Path) -> io::Result<Option<Truncation>> {
    match fs::read_to_string(dir.join(TRUNCATED)) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse(line: &str) -> Option<LogRecord> {
    serde_json::from_str(line).ok()
}

/// How many bytes everything under `path` takes, 0 if it doesn't exist.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| disk_usage(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::RunId;

    fn key() -> LogKey {
        LogKey {
            run: RunId(1),
            step: "ci".into(),
            attempt: 1,
        }
    }

    fn writer(dir: &Path, limits: Limits) -> Writer {
        Writer::create(dir.join("log"), key(), limits, Hub::default()).unwrap()
    }

    fn texts(records: &[LogRecord]) -> Vec<&str> {
        records.iter().map(|record| record.text.as_str()).collect()
    }

    fn page(before: Option<u64>, after: Option<u64>, limit: u32) -> LogPage {
        LogPage {
            before,
            after,
            limit: Some(limit),
        }
    }

    fn all(dir: &Path) -> Page {
        read_page(&dir.join("log"), LogPage::default(), &LogFilter::default()).unwrap()
    }

    #[test]
    fn records_read_back_in_order_with_their_source_and_level() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = writer(dir.path(), Limits::default());

        log.write(LogSource::Stderr, None, "compiling");
        log.write(LogSource::Log, Some(LogLevel::Warn), "slow");

        let page = all(dir.path());
        assert_eq!(texts(&page.records), ["compiling", "slow"]);
        assert_eq!(page.records[0].seq, 1);
        assert_eq!(page.records[1].seq, 2);
        assert_eq!(page.records[1].source, LogSource::Log);
        assert_eq!(page.records[1].level, Some(LogLevel::Warn));
        assert!(page.records[0].ts > 0);
        assert!(!page.more_before && !page.more_after);
        assert_eq!(page.truncated, None);
    }

    #[test]
    fn a_line_longer_than_the_limit_becomes_several_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = writer(
            dir.path(),
            Limits {
                line: 4,
                ..Limits::default()
            },
        );

        log.write(LogSource::Stderr, None, "abcdéfgh");

        assert_eq!(texts(&all(dir.path()).records), ["abcd", "éfg", "h"]);
    }

    #[test]
    fn pages_go_backward_and_forward_through_a_log_larger_than_a_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = writer(dir.path(), Limits::default());
        for n in 1..=10 {
            log.write(LogSource::Stderr, None, &format!("line {n}"));
        }
        let read = |page| read_page(&dir.path().join("log"), page, &LogFilter::default()).unwrap();

        let last = read(page(None, None, 3));
        assert_eq!(texts(&last.records), ["line 8", "line 9", "line 10"]);
        assert!(last.more_before && !last.more_after);

        let older = read(page(Some(8), None, 3));
        assert_eq!(texts(&older.records), ["line 5", "line 6", "line 7"]);
        assert!(older.more_before && older.more_after);

        let first = read(page(Some(3), None, 3));
        assert_eq!(texts(&first.records), ["line 1", "line 2"]);
        assert!(!first.more_before && first.more_after);

        let newer = read(page(None, Some(2), 3));
        assert_eq!(texts(&newer.records), ["line 3", "line 4", "line 5"]);
        assert!(newer.more_before && newer.more_after);
    }

    #[test]
    fn search_and_filters_page_through_matches_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = writer(dir.path(), Limits::default());
        for n in 1..=50 {
            if n % 10 == 0 {
                log.write(
                    LogSource::Log,
                    Some(LogLevel::Error),
                    &format!("FAILED {n}"),
                );
            } else {
                log.write(LogSource::Stderr, None, &format!("ok {n}"));
            }
        }
        let search = LogFilter {
            search: Some("failed".into()),
            ..LogFilter::default()
        };
        let read =
            |page, filter: &LogFilter| read_page(&dir.path().join("log"), page, filter).unwrap();

        let last = read(page(None, None, 2), &search);
        assert_eq!(texts(&last.records), ["FAILED 40", "FAILED 50"]);
        assert!(last.more_before && !last.more_after);
        let before = read(page(Some(last.records[0].seq), None, 2), &search);
        assert_eq!(texts(&before.records), ["FAILED 20", "FAILED 30"]);
        assert!(before.more_before && before.more_after);

        let only_log = LogFilter {
            sources: vec![LogSource::Log],
            ..LogFilter::default()
        };
        assert_eq!(read(page(None, None, 100), &only_log).records.len(), 5);
        let errors = LogFilter {
            sources: vec![LogSource::Log],
            levels: vec![LogLevel::Info],
            ..LogFilter::default()
        };
        assert!(read(page(None, None, 100), &errors).records.is_empty());
    }

    #[test]
    fn past_the_cap_the_log_keeps_its_head_and_a_rolling_tail_and_counts_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        // Each record is about 60 bytes of JSON: a head of 3, then tail
        // segments of 2, at most 2 segments.
        let limits = Limits {
            head: 200,
            tail: 260,
            segment: 130,
            line: 1000,
        };
        let mut log = writer(dir.path(), limits);
        for n in 1..=30 {
            log.write(LogSource::Stderr, None, &format!("line {n:02}"));
        }

        let page = all(dir.path());
        let seqs: Vec<u64> = page.records.iter().map(|record| record.seq).collect();
        assert_eq!(&seqs[..3], [1, 2, 3], "the head stays: {seqs:?}");
        assert_eq!(*seqs.last().unwrap(), 30, "the tail keeps the newest");
        let truncated = page.truncated.expect("the log records the truncation");
        assert_eq!(truncated.after_seq, 3);
        assert_eq!(
            truncated.records as usize,
            30 - seqs.len(),
            "every dropped record is counted"
        );
        assert_eq!(seqs[3], 3 + truncated.records + 1, "the gap is contiguous");
        assert!(truncated.bytes > 0);
        assert!(disk_usage(&dir.path().join("log")) < 200 + 260 + 130 + 100);

        let after_gap = read_page(
            &dir.path().join("log"),
            page_after(2),
            &LogFilter::default(),
        )
        .unwrap();
        assert_eq!(after_gap.records[0].seq, 3);
        assert_eq!(after_gap.records[1].seq, seqs[3]);
    }

    fn page_after(after: u64) -> LogPage {
        page(None, Some(after), 2)
    }

    #[test]
    fn written_records_reach_live_subscribers_after_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::default();
        let mut live = hub.subscribe();
        let mut log =
            Writer::create(dir.path().join("log"), key(), Limits::default(), hub).unwrap();

        log.write(LogSource::Stderr, None, "hello");

        let (got_key, record) = live.try_recv().unwrap();
        assert_eq!(got_key, key());
        assert_eq!(record.text, "hello");
        assert_eq!(
            read_after(&dir.path().join("log"), 0, 10).unwrap(),
            [record]
        );
    }

    #[test]
    fn reading_after_gives_at_most_the_latest_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = writer(dir.path(), Limits::default());
        for n in 1..=10 {
            log.write(LogSource::Stderr, None, &format!("{n}"));
        }

        let log_dir = dir.path().join("log");
        assert_eq!(texts(&read_after(&log_dir, 8, 5).unwrap()), ["9", "10"]);
        assert_eq!(
            texts(&read_after(&log_dir, 0, 3).unwrap()),
            ["8", "9", "10"]
        );
        assert!(
            read_after(&dir.path().join("missing"), 0, 3)
                .unwrap()
                .is_empty()
        );
    }
}
