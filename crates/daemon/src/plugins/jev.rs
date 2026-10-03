//! The built-in `jev` Plugin. It asks TypeSafe's Jev, through the Vercel AI
//! Gateway's `/v1/evaluate`, boolean questions about the PR, and passes when
//! every answer is the one its question passes on.
//!
//! Each Step sends one request: the questions, and a state built from the
//! evidence its `with:` names (the description, the linked issues, the
//! diff). Jev reads at most 32k tokens of state plus the longest question,
//! so the Plugin measures the state before sending it. A diff too large for
//! that loses its lockfiles, generated and binary files first, and then
//! every file is cut to an even share of what's left, with a list of every
//! changed file kept whole.
//!
//! Every request asks for zero data retention and only the `typesafe-ai`
//! provider. A gateway that can't honor that refuses the request, and the
//! Step errors rather than asking again without them (#18).
//!
//! Rate limits are unstable and arrive as 429s without `Retry-After`, so
//! the Plugin retries 429s and 5xx with jittered backoff from 1 to 30 s, and
//! the manifest caps the Plugin at 4 Steps at once, one request each.
//! Parallax lost a quarter of its calls at 8 in flight and none at 4.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{
    Finding, FromStep, Manifest, Outcome, Outputs, PR_DIFF, PrSnapshot, STEP_DIALECT, SecretSpec,
    Severity, Start, ToStep, Usage,
};

/// The Secret the Step calls the AI Gateway with.
pub const KEY_SECRET: &str = "AI_GATEWAY_API_KEY";

pub const ENDPOINT: &str = "https://ai-gateway.vercel.sh/v1/evaluate";

pub const MODEL: &str = "typesafe-ai/jev";

/// Jev's limit on state plus the longest question, in tokens.
pub const TOKEN_CAP: usize = 32_000;

/// How many bytes count as one token when measuring. Jev's tokenizer is
/// unpublished, and code packs more tokens per byte than prose, so this is
/// on the low side.
const BYTES_PER_TOKEN: usize = 3;

/// Tokens kept free for the estimate's error.
const MARGIN_TOKENS: usize = 2_000;

/// How long one request may take. Jev usually answers within a second,
/// with a tail up to about 18 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The waits before each retry. A wait gets up to a quarter added at
/// random so Steps that hit a 429 together don't retry together.
pub const BACKOFF: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

/// At most this many `jev` Steps run at once, across every Run.
pub const CONCURRENCY: u32 = 4;

/// A Step's default timeout: one request, with every retry and its wait.
pub const TIMEOUT: &str = "5m";

pub fn manifest() -> Manifest {
    Manifest {
        id: "jev".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![PR_DIFF.into()],
        config_schema: json!({
            "type": "object",
            "properties": {
                "evidence": {
                    "type": "array",
                    "items": { "enum": ["description", "linked_issue", "diff"] },
                },
                "confidence": { "type": "number", "exclusiveMinimum": 0.5, "maximum": 1 },
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "question": { "type": "string" },
                            "pass_when": { "type": "boolean" },
                            "true_when": { "type": "string" },
                            "false_when": { "type": "string" },
                        },
                        "required": ["id", "question", "pass_when"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["questions"],
            "additionalProperties": false,
        }),
        workspace: Workspace::None,
        effects: vec![],
        secrets: vec![SecretSpec::required(KEY_SECRET)],
        timeout: Some(TIMEOUT.into()),
        stall_after: None,
        // Cost budgets: a Jev call costs about a tenth of a cent.
        budget_usd: Some(0.05),
        concurrency: Some(CONCURRENCY),
    }
}

/// The Step's `with:`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What Jev reads besides the PR's title. Less is better: Jev gets
    /// less accurate as the state fills with what a question doesn't need.
    #[serde(default = "default_evidence")]
    pub evidence: Vec<Evidence>,
    /// How sure Jev must be of an answer for it to count. Between that and
    /// its opposite, the question is inconclusive.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    pub questions: Vec<Question>,
}

fn default_evidence() -> Vec<Evidence> {
    vec![Evidence::Description, Evidence::Diff]
}

fn default_confidence() -> f64 {
    0.7
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The PR's description.
    Description,
    /// The issues the PR says it closes.
    LinkedIssue,
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// The question's name in Findings and the request.
    pub id: String,
    /// A yes-or-no question about the PR.
    pub question: String,
    /// The answer the question passes on.
    pub pass_when: bool,
    /// What a yes means, for a question Jev might read too literally.
    #[serde(default)]
    pub true_when: Option<String>,
    /// What a no means.
    #[serde(default)]
    pub false_when: Option<String>,
}

impl Config {
    fn check(&self) -> Result<(), String> {
        if self.questions.is_empty() {
            return Err("`questions` needs at least one question".into());
        }
        if !(self.confidence > 0.5 && self.confidence <= 1.0) {
            return Err(format!(
                "`confidence` must be above 0.5 and at most 1, not {}",
                self.confidence
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for question in &self.questions {
            if !ids.insert(&question.id) {
                return Err(format!("two questions have the id `{}`", question.id));
            }
        }
        Ok(())
    }

    fn wants(&self, evidence: Evidence) -> bool {
        self.evidence.contains(&evidence)
    }
}

/// The state a request sends, and what had to go to fit it under the cap.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub value: Value,
    /// What was left out or cut, for the note and the Step log.
    pub condensed: Option<String>,
}

/// One file's part of a unified diff.
struct FileDiff<'a> {
    path: String,
    text: &'a str,
    added: usize,
    removed: usize,
    /// A lockfile, generated or binary file: left out first.
    noise: bool,
}

/// Builds the state for `config`'s questions about `snapshot`, small
/// enough that state plus the longest question stays under
/// [`TOKEN_CAP`]. `diff` is the PR's unified diff when the evidence names
/// it.
pub fn build_state(
    config: &Config,
    snapshot: &PrSnapshot,
    diff: Option<&str>,
) -> Result<State, String> {
    let budget = state_budget(config);
    let mut state = Map::new();
    state.insert("title".into(), json!(snapshot.title));
    if config.wants(Evidence::Description) {
        state.insert("description".into(), json!(snapshot.body));
    }
    if config.wants(Evidence::LinkedIssue) {
        let issues: Vec<Value> = snapshot
            .linked_issues
            .iter()
            .map(|issue| {
                json!({
                    "issue": format!("{}#{}", issue.repo, issue.number),
                    "title": issue.title,
                    "body": issue.body,
                })
            })
            .collect();
        state.insert("linked_issues".into(), Value::Array(issues));
    }
    let Some(diff) = diff.filter(|_| config.wants(Evidence::Diff)) else {
        return fit_texts(state, budget, None);
    };

    let mut whole = state.clone();
    whole.insert("diff".into(), json!(diff));
    if size_of(&whole) <= budget {
        return Ok(State {
            value: Value::Object(whole),
            condensed: None,
        });
    }

    let files = split_diff(diff);
    let listing: Vec<String> = files
        .iter()
        .map(|file| format!("{} (+{} -{})", file.path, file.added, file.removed))
        .collect();
    let shown: Vec<&FileDiff> = files.iter().filter(|file| !file.noise).collect();
    let left_out = files.len() - shown.len();
    state.insert("files".into(), json!(listing));
    let mut note = format!(
        "The whole diff is {} KB, more than Jev reads at once, so `files` lists every changed \
         file and `diff` holds part of it.",
        diff.len().div_ceil(1024)
    );
    if left_out > 0 {
        note.push_str(&format!(
            " {left_out} lockfile, generated or binary files are left out of `diff`."
        ));
    }
    state.insert("diff_note".into(), json!(note));
    state.insert("diff".into(), json!(""));

    let mut room = budget.saturating_sub(size_of(&state));
    let mut cut = 0;
    // JSON escaping makes the text a little longer than its bytes, so the
    // room shrinks by the overshoot until the state fits.
    for _ in 0..8 {
        let (text, files_cut) = fit_files(&shown, room);
        cut = files_cut;
        state.insert("diff".into(), json!(text));
        let over = size_of(&state).saturating_sub(budget);
        if over == 0 {
            break;
        }
        room = room.saturating_sub(over + 512);
    }
    if cut > 0 {
        let note = format!(
            "{note} {cut} of the {} files shown are cut short to fit.",
            shown.len()
        );
        state.insert("diff_note".into(), json!(note));
    }
    let summary = match (left_out, cut) {
        (0, 0) => "diff condensed to fit Jev's limit".to_owned(),
        _ => {
            format!("diff condensed to fit Jev's limit: {left_out} files left out, {cut} cut short")
        }
    };
    fit_texts(state, budget, Some(summary))
}

/// The state as it is, or with its long texts cut when even they don't
/// fit, as with a description pasted with a whole log.
fn fit_texts(
    mut state: Map<String, Value>,
    budget: usize,
    note: Option<String>,
) -> Result<State, String> {
    if size_of(&state) <= budget {
        return Ok(State {
            value: Value::Object(state),
            condensed: note,
        });
    }
    let cap = budget / 8;
    let mut cut = false;
    for (key, value) in state.iter_mut() {
        if key == "diff" {
            continue;
        }
        cut |= cut_strings(value, cap);
    }
    if let Some(Value::Array(files)) = state.get_mut("files")
        && size_of(&Value::Array(files.clone())) > cap
    {
        let total = files.len();
        while files.len() > 1 && size_of(&Value::Array(files.clone())) > cap {
            files.truncate(files.len() / 2);
        }
        files.push(json!(format!("... and {} more files", total - files.len())));
        cut = true;
    }
    if size_of(&state) > budget {
        // Whatever is still too long is the diff, which gets what's left.
        if let Some(Value::String(diff)) = state.get("diff").cloned() {
            let over = size_of(&state) - budget;
            let keep = diff.len().saturating_sub(over + 512);
            state.insert("diff".into(), json!(cut_text(&diff, keep)));
        }
    }
    if size_of(&state) > budget {
        return Err(format!(
            "the PR's evidence doesn't fit in Jev's {TOKEN_CAP}-token limit even cut down"
        ));
    }
    let note = match (note, cut) {
        (Some(note), true) => Some(format!("{note}; long texts cut short")),
        (None, true) => Some("long texts cut short to fit Jev's limit".to_owned()),
        (note, false) => note,
    };
    Ok(State {
        value: Value::Object(state),
        condensed: note,
    })
}

/// Cuts every string in `value` longer than `cap` bytes. Returns whether
/// any was.
fn cut_strings(value: &mut Value, cap: usize) -> bool {
    match value {
        Value::String(text) if text.len() > cap => {
            *text = cut_text(text, cap);
            true
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |cut, item| cut_strings(item, cap) | cut),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |cut, item| cut_strings(item, cap) | cut),
        _ => false,
    }
}

/// The first `keep` bytes of `text`, ended at a line where there's one,
/// and a line saying how much was cut.
fn cut_text(text: &str, keep: usize) -> String {
    if text.len() <= keep {
        return text.to_owned();
    }
    let mut end = keep;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = text[..end].rfind('\n') {
        end = newline + 1;
    }
    let cut_lines = text[end..].lines().count();
    let mut kept = text[..end].to_owned();
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    kept.push_str(&format!("[... {cut_lines} more lines cut to fit]\n"));
    kept
}

/// The files' diffs together in about `room` bytes: each file gets an even
/// share, and what a small file doesn't use goes to the larger ones.
/// Returns the text and how many files were cut.
fn fit_files(files: &[&FileDiff], room: usize) -> (String, usize) {
    // Room for each cut file's closing line.
    const MARKER: usize = 48;
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&index| files[index].text.len());
    let mut shares = vec![0; files.len()];
    let mut left = room;
    for (taken, &index) in order.iter().enumerate() {
        let fair = left / (files.len() - taken);
        let size = files[index].text.len();
        let share = if size <= fair {
            size
        } else {
            fair.saturating_sub(MARKER)
        };
        shares[index] = share;
        left -= share.min(left);
        if size > fair {
            left = left.saturating_sub(MARKER);
        }
    }
    let mut text = String::new();
    let mut cut = 0;
    for (file, share) in files.iter().zip(shares) {
        if share >= file.text.len() {
            text.push_str(file.text);
        } else {
            cut += 1;
            text.push_str(&cut_text(file.text, share));
        }
    }
    (text, cut)
}

/// Splits a unified diff at each `diff --git` header.
fn split_diff(diff: &str) -> Vec<FileDiff<'_>> {
    let mut starts: Vec<usize> = diff
        .match_indices("diff --git ")
        .map(|(at, _)| at)
        .filter(|&at| at == 0 || diff.as_bytes()[at - 1] == b'\n')
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(diff.len());
            let text = &diff[start..end];
            let header = text.lines().next().unwrap_or_default();
            let path = header
                .rsplit_once(" b/")
                .map_or(header, |(_, path)| path)
                .to_owned();
            let (mut added, mut removed) = (0, 0);
            for line in text.lines().skip(1) {
                if line.starts_with('+') && !line.starts_with("+++ ") {
                    added += 1;
                } else if line.starts_with('-') && !line.starts_with("--- ") {
                    removed += 1;
                }
            }
            let binary = text.contains("\nBinary files ") || text.contains("\nGIT binary patch");
            FileDiff {
                noise: binary || is_generated(&path),
                path,
                text,
                added,
                removed,
            }
        })
        .collect()
}

/// Lockfiles and generated files, which say little about what a PR does.
fn is_generated(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "go.sum"
            | "poetry.lock"
            | "uv.lock"
            | "Gemfile.lock"
            | "composer.lock"
            | "flake.lock"
            | "Package.resolved"
    ) || [
        ".lock",
        ".min.js",
        ".min.css",
        ".map",
        ".snap",
        ".pb.go",
        ".generated.ts",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

/// The bytes the state may take: the token cap, less the longest question
/// and a margin, at [`BYTES_PER_TOKEN`].
fn state_budget(config: &Config) -> usize {
    let longest = config
        .questions
        .iter()
        .map(|question| size_of(&question_json(question, &[])))
        .max()
        .unwrap_or_default();
    let tokens = TOKEN_CAP - MARGIN_TOKENS - longest.div_ceil(BYTES_PER_TOKEN).min(TOKEN_CAP / 2);
    tokens * BYTES_PER_TOKEN
}

/// The bytes `value` takes as JSON.
fn size_of(value: &impl serde::Serialize) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
}

/// One question as `/v1/evaluate` takes it. `inspect` names the state's
/// fields, so Jev knows where to look.
fn question_json(question: &Question, inspect: &[&str]) -> Value {
    let mut instructions = json!({ "question": question.question });
    if !inspect.is_empty() {
        instructions["inspect"] = json!(inspect);
    }
    let mut value = json!({ "type": "boolean", "instructions": instructions });
    let mut criteria = Map::new();
    if let Some(yes) = &question.true_when {
        criteria.insert("true".into(), json!(yes));
    }
    if let Some(no) = &question.false_when {
        criteria.insert("false".into(), json!(no));
    }
    if !criteria.is_empty() {
        value["criteria"] = Value::Object(criteria);
    }
    value
}

/// The whole request. Zero data retention and the provider are fixed here,
/// never settings (#18).
pub fn request(config: &Config, state: &State) -> Value {
    let inspect: Vec<&str> = state
        .value
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let questions: Map<String, Value> = config
        .questions
        .iter()
        .map(|question| (question.id.clone(), question_json(question, &inspect)))
        .collect();
    json!({
        "model": MODEL,
        "state": state.value,
        "questions": questions,
        "providerOptions": {
            "gateway": { "zeroDataRetention": true, "only": ["typesafe-ai"] },
        },
    })
}

/// What Jev answered, already checked against the questions.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// P(true) for each question, by id.
    pub answers: BTreeMap<String, f64>,
    pub warnings: Vec<String>,
    pub usage: Usage,
    pub generation: Option<String>,
}

impl Reply {
    /// Reads a 200 answer to `config`'s questions. Every question needs a
    /// probability between 0 and 1, and no other answer may come.
    pub fn read(config: &Config, body: &Value) -> Result<Reply, String> {
        let answers = body
            .get("answers")
            .and_then(Value::as_object)
            .ok_or("the answer has no `answers`")?;
        let mut read = BTreeMap::new();
        for question in &config.questions {
            let probability = answers
                .get(&question.id)
                .and_then(|answer| answer.get("probability"))
                .and_then(Value::as_f64)
                .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                .ok_or_else(|| format!("no usable answer to `{}`", question.id))?;
            read.insert(question.id.clone(), probability);
        }
        if answers.len() != read.len() {
            return Err("answers to questions it wasn't asked".into());
        }
        let gateway = body.pointer("/providerMetadata/gateway");
        let cost = gateway.and_then(|gateway| gateway.get("cost"));
        let usd = match cost {
            Some(Value::String(text)) => text.parse().ok(),
            Some(value) => value.as_f64(),
            None => None,
        };
        let tokens = |key: &str| {
            body.pointer(&format!("/usage/{key}"))
                .and_then(Value::as_u64)
                .unwrap_or_default()
        };
        let warnings = match body.get("warnings") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(Reply {
            answers: read,
            warnings,
            usage: Usage {
                model: MODEL.into(),
                input_tokens: tokens("inputTokens"),
                cached_input_tokens: 0,
                output_tokens: tokens("outputTokens"),
                usd,
            },
            generation: gateway
                .and_then(|gateway| gateway.get("generationId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// The Outcome for Jev's answers: pass when every question passed, fail
/// when any failed, and inconclusive otherwise. A reply with warnings
/// can't be trusted, so it's inconclusive whatever it says.
pub fn judge(config: &Config, reply: &Reply, condensed: Option<&str>) -> Outcome {
    let mut findings = Vec::new();
    let mut lowest: Option<f64> = None;
    let (mut failed, mut unsure) = (0, 0);
    for question in &config.questions {
        let p_true = reply.answers[&question.id];
        let p_pass = if question.pass_when {
            p_true
        } else {
            1.0 - p_true
        };
        lowest = Some(lowest.map_or(p_pass, |low| low.min(p_pass)));
        let answer = if p_true >= 0.5 { "yes" } else { "no" };
        let sure = (p_true.max(1.0 - p_true) * 100.0).round();
        let wanted = if question.pass_when { "yes" } else { "no" };
        if p_pass >= config.confidence {
            continue;
        }
        let (severity, verdict) = if 1.0 - p_pass >= config.confidence {
            failed += 1;
            (Severity::Error, "fails")
        } else {
            unsure += 1;
            (Severity::Warning, "is inconclusive")
        };
        findings.push(Finding {
            severity,
            message: format!(
                "{} Jev answered {answer} ({sure}% sure), and it passes on {wanted}, so `{}` \
                 {verdict}.",
                question.question, question.id
            ),
            file: None,
            line: None,
        });
    }
    let total = config.questions.len();
    let (verdict, mut note) = if !reply.warnings.is_empty() {
        // The answers don't count, so neither do the Findings they made.
        findings = vec![Finding {
            severity: Severity::Warning,
            message: format!(
                "Jev warned about its answers, so they don't count: {}",
                reply.warnings.join("; ")
            ),
            file: None,
            line: None,
        }];
        (
            Verdict::Inconclusive,
            "Jev warned about its answers".to_owned(),
        )
    } else {
        match (failed, unsure) {
            (0, 0) => (
                Verdict::Pass,
                format!("{total} of {total} questions passed"),
            ),
            (0, unsure) => (
                Verdict::Inconclusive,
                format!("{unsure} of {total} questions inconclusive"),
            ),
            (failed, _) => (
                Verdict::Fail,
                format!("{failed} of {total} questions failed"),
            ),
        }
    };
    if let Some(condensed) = condensed {
        note.push_str(&format!("; {condensed}"));
    }
    Outcome {
        verdict,
        outputs: Outputs {
            findings,
            note: Some(note),
            probability: lowest,
            ..Outputs::default()
        },
    }
}

/// Why a request got no answer. Each one errors the Step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevError {
    /// The gateway can't route the request to a provider with zero data
    /// retention. It is never sent again without it.
    NoZdr(String),
    /// The gateway or Jev refused the request, such as for a rejected Secret or a
    /// spent gateway budget.
    Refused { status: u16, body: String },
    /// It kept failing for reasons a retry could fix, until the retries
    /// ran out.
    Unavailable(String),
}

impl std::fmt::Display for JevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JevError::NoZdr(body) => write!(
                f,
                "the AI Gateway has no zero-data-retention route to Jev, so nothing was sent \
                 without it: {body}"
            ),
            JevError::Refused { status, body } => {
                write!(f, "the AI Gateway refused the request ({status}): {body}")
            }
            JevError::Unavailable(why) => write!(f, "Jev didn't answer: {why}"),
        }
    }
}

impl std::error::Error for JevError {}

/// The AI Gateway's `/v1/evaluate`.
pub struct Gateway {
    http: reqwest::Client,
    endpoint: String,
    key: String,
    backoff: Vec<Duration>,
}

impl Gateway {
    pub fn new(
        endpoint: impl Into<String>,
        key: impl Into<String>,
        backoff: Vec<Duration>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("slopwatch/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build the HTTP client");
        Self {
            http,
            endpoint: endpoint.into(),
            key: key.into(),
            backoff,
        }
    }

    /// Sends `body`, retrying 429s, 5xx and network failures after each
    /// wait in the backoff, and returns the 200 answer.
    pub async fn evaluate(&self, body: &Value) -> Result<Value, JevError> {
        let mut waits = self.backoff.iter();
        loop {
            let why = match self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.key)
                .json(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let text = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        return serde_json::from_str(&text).map_err(|error| {
                            JevError::Unavailable(format!("an unreadable answer: {error}"))
                        });
                    }
                    let excerpt = excerpt(&text);
                    if text.contains("no_zdr_providers_available") {
                        return Err(JevError::NoZdr(excerpt));
                    }
                    if !retryable(status.as_u16()) {
                        return Err(JevError::Refused {
                            status: status.as_u16(),
                            body: excerpt,
                        });
                    }
                    (format!("{status}: {excerpt}"), retry_after)
                }
                Err(error) => (error.to_string(), None),
            };
            let (why, retry_after) = why;
            let Some(wait) = waits.next() else {
                return Err(JevError::Unavailable(why));
            };
            let wait = retry_after
                .map(|after| after.min(self.backoff.iter().copied().max().unwrap_or(after)))
                .unwrap_or_else(|| jitter(*wait));
            eprintln!("jev: {why}; asking again in {:.1}s", wait.as_secs_f64());
            tokio::time::sleep(wait).await;
        }
    }
}

/// A timeout (408), a rate limit (429), the gateway's and Jev's 5xx, and
/// TypeSafe's 529 "overloaded".
fn retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Up to a quarter more than `wait`, from the clock's nanoseconds.
fn jitter(wait: Duration) -> Duration {
    // macOS clocks tick in microseconds, and the pid sets apart Steps
    // whose retries come due in the same one.
    let micros = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |now| now.subsec_micros());
    let spread = (u64::from(micros) + u64::from(std::process::id()) * 7919) % 1000;
    wait + wait.mul_f64(spread as f64 / 4000.0)
}

/// The start of an error body, enough to say what went wrong.
fn excerpt(text: &str) -> String {
    let text = text.trim();
    let mut end = text.len().min(400);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Runs one session with the AI Gateway Secret from the Step's environment.
pub fn run(input: impl BufRead + Send + 'static, output: impl Write) -> std::io::Result<()> {
    let key = std::env::var(KEY_SECRET).ok();
    session(input, output, key, |key| {
        Gateway::new(ENDPOINT, key, BACKOFF.to_vec())
    })
}

/// Runs one session: reads `start`, asks Jev once with the gateway `key`,
/// and writes the usage and the Outcome. A `cancel` while the request is
/// out ends it with nothing more said. A request that can't be answered is
/// an error the Step exits with, so the daemon errors the Step.
pub fn session(
    mut input: impl BufRead + Send + 'static,
    mut output: impl Write,
    key: Option<String>,
    gateway: impl FnOnce(String) -> Gateway,
) -> std::io::Result<()> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let start = match serde_json::from_str::<ToStep>(&line) {
        Ok(ToStep::Start(start)) => start,
        Ok(ToStep::Cancel) => return Ok(()),
        Ok(other) => return Err(invalid(format!("expected `start`, got {other:?}"))),
        Err(error) => return Err(invalid(format!("can't read `start`: {error}"))),
    };
    let config = match read_config(&start) {
        Ok(config) => config,
        Err(error) => {
            let outcome = fail(format!("The Step's `with:` doesn't read: {error}"));
            return send(&mut output, &FromStep::Outcome(outcome));
        }
    };
    if config.wants(Evidence::LinkedIssue) && start.snapshot.linked_issues.is_empty() {
        let outcome = Outcome {
            verdict: Verdict::Inconclusive,
            outputs: Outputs {
                findings: vec![Finding {
                    severity: Severity::Info,
                    message: "The PR links no issue, so there's nothing to check it against. \
                              Run this Step `when: { linked_issue: true }` to skip it instead."
                        .into(),
                    file: None,
                    line: None,
                }],
                note: Some("The PR links no issue".into()),
                ..Outputs::default()
            },
        };
        return send(&mut output, &FromStep::Outcome(outcome));
    }
    let diff = if config.wants(Evidence::Diff) {
        let path = start
            .snapshot
            .diff
            .as_deref()
            .ok_or_else(|| invalid("the daemon sent no diff".into()))?;
        Some(read_diff(path)?)
    } else {
        None
    };
    let state = build_state(&config, &start.snapshot, diff.as_deref()).map_err(invalid)?;
    if let Some(condensed) = &state.condensed {
        eprintln!("jev: {condensed}");
    }
    let body = request(&config, &state);
    let estimate = size_of(&body).div_ceil(BYTES_PER_TOKEN);
    eprintln!(
        "jev: asking {} questions over about {estimate} tokens",
        config.questions.len()
    );
    send(
        &mut output,
        &FromStep::Progress {
            message: Some(format!("Asking Jev {} questions", config.questions.len())),
        },
    )?;

    let key = key.ok_or_else(|| invalid(format!("`{KEY_SECRET}` isn't in the environment")))?;
    let gateway = gateway(key);
    let cancelled = watch_for_cancel(input);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let answer = runtime.block_on(async {
        tokio::select! {
            answer = gateway.evaluate(&body) => Some(answer),
            Ok(()) = cancelled => None,
        }
    });
    let Some(answer) = answer else {
        return Ok(());
    };
    let answer = answer.map_err(std::io::Error::other)?;
    let reply = Reply::read(&config, &answer)
        .map_err(|why| std::io::Error::other(format!("Jev's answer doesn't read: {why}")))?;
    eprintln!(
        "jev: {} input tokens (estimated {estimate}), generation {}",
        reply.usage.input_tokens,
        reply.generation.as_deref().unwrap_or("unknown")
    );
    send(&mut output, &FromStep::Usage(reply.usage.clone()))?;
    let outcome = judge(&config, &reply, state.condensed.as_deref());
    send(&mut output, &FromStep::Outcome(outcome))
}

fn read_config(start: &Start) -> Result<Config, String> {
    let config: Config = serde_json::from_value(Value::Object(start.config.clone()))
        .map_err(|error| error.to_string())?;
    config.check()?;
    Ok(config)
}

fn read_diff(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("can't read the diff at {}: {error}", path.display()),
        )
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Resolves once the daemon sends `cancel`. The rest of the input is read
/// on a thread of its own, since stdin can't be read asynchronously.
fn watch_for_cancel(input: impl BufRead + Send + 'static) -> tokio::sync::oneshot::Receiver<()> {
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if matches!(serde_json::from_str(&line), Ok(ToStep::Cancel)) {
                let _ = cancel.send(());
                return;
            }
        }
        // The daemon hung up without cancelling. Dropping `cancel` closes
        // the channel, which isn't a cancel, so the request goes on.
    });
    cancelled
}

fn invalid(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn fail(message: String) -> Outcome {
    Outcome {
        verdict: Verdict::Fail,
        outputs: Outputs {
            findings: vec![Finding {
                severity: Severity::Error,
                message,
                file: None,
                line: None,
            }],
            ..Outputs::default()
        },
    }
}

fn send(output: &mut impl Write, message: &FromStep) -> std::io::Result<()> {
    let line = serde_json::to_string(message)?;
    writeln!(output, "{line}")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use slopwatch_core::{PluginInfo, Resolver};
    use slopwatch_protocol::RepoName;
    use slopwatch_protocol::step::{Checks, LinkedIssue};

    fn snapshot() -> PrSnapshot {
        PrSnapshot {
            repo: RepoName::new("o", "r"),
            number: 7,
            title: "Fix the crash on empty input".into(),
            body: "Empty input crashed the parser. This returns an error instead.".into(),
            url: String::new(),
            author: "me".into(),
            head_sha: "abc".into(),
            base: "main".into(),
            draft: false,
            labels: vec![],
            checks: Checks::default(),
            merge: None,
            diff: None,
            linked_issues: vec![],
            stacked_on: None,
        }
    }

    fn issue() -> LinkedIssue {
        LinkedIssue {
            repo: RepoName::new("o", "r"),
            number: 3,
            title: "Parser crashes on empty input".into(),
            body: "Steps: run it with no input.".into(),
            url: "https://github.com/o/r/issues/3".into(),
        }
    }

    fn question(id: &str, pass_when: bool) -> Question {
        Question {
            id: id.into(),
            question: format!("Is {id} so?"),
            pass_when,
            true_when: None,
            false_when: None,
        }
    }

    fn config(questions: Vec<Question>) -> Config {
        Config {
            evidence: default_evidence(),
            confidence: default_confidence(),
            questions,
        }
    }

    /// A file's diff with `lines` added lines of about 70 bytes each, the
    /// size the Jev research measured per changed line.
    fn file_diff(path: &str, lines: usize) -> String {
        let mut text = format!(
            "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n@@ -1,0 +1,{lines} @@\n"
        );
        for n in 0..lines {
            text.push_str(&format!(
                "+    let value_{n:05} = compute_something(input, {n}).unwrap_or_default();\n"
            ));
        }
        text
    }

    fn tokens(value: &Value) -> usize {
        size_of(value).div_ceil(BYTES_PER_TOKEN)
    }

    #[test]
    fn a_small_diff_goes_whole() {
        let diff = file_diff("src/parse.rs", 40);

        let state =
            build_state(&config(vec![question("q", true)]), &snapshot(), Some(&diff)).unwrap();

        assert_eq!(state.condensed, None);
        assert_eq!(state.value["diff"], json!(diff));
        assert_eq!(state.value["title"], json!("Fix the crash on empty input"));
        assert!(state.value.get("linked_issues").is_none(), "not asked for");
    }

    #[test]
    fn a_p90_sized_pr_is_condensed_under_the_token_cap() {
        // The research's p90 PR: 3,381 changed lines, about 230 KB, here
        // with a lockfile that takes a third of it.
        let mut diff = file_diff("Cargo.lock", 1100);
        for n in 0..9 {
            diff.push_str(&file_diff(&format!("src/module_{n}.rs"), 250));
        }
        diff.push_str(&file_diff("src/small.rs", 31));
        assert!(diff.len() > 220_000, "{}", diff.len());
        let config = config(vec![
            question("does-what-it-says", true),
            question("unmentioned-changes", false),
        ]);

        let state = build_state(&config, &snapshot(), Some(&diff)).unwrap();
        let body = request(&config, &state);

        let longest = config
            .questions
            .iter()
            .map(|q| tokens(&question_json(q, &[])))
            .max()
            .unwrap();
        assert!(
            tokens(&state.value) + longest < TOKEN_CAP,
            "{} tokens",
            tokens(&state.value)
        );
        assert!(tokens(&body) < TOKEN_CAP, "{} tokens", tokens(&body));
        let shown = state.value["diff"].as_str().unwrap();
        assert!(!shown.contains("Cargo.lock"), "the lockfile goes first");
        assert!(
            shown.contains(&file_diff("src/small.rs", 31)),
            "small files stay whole"
        );
        for n in 0..9 {
            assert!(
                shown.contains(&format!("b/src/module_{n}.rs")),
                "every file shows"
            );
        }
        assert!(shown.contains("more lines cut to fit"));
        let files = state.value["files"].as_array().unwrap();
        assert_eq!(files.len(), 11);
        assert_eq!(files[0], json!("Cargo.lock (+1100 -0)"));
        let condensed = state.condensed.unwrap();
        assert!(
            condensed.contains("1 files left out, 9 cut short"),
            "{condensed}"
        );
        assert!(
            state.value["diff_note"]
                .as_str()
                .unwrap()
                .contains("lockfile")
        );
    }

    #[test]
    fn a_huge_pr_still_fits_with_its_texts_cut() {
        let mut diff = String::new();
        for n in 0..3000 {
            diff.push_str(&file_diff(&format!("src/generated/part_{n}.rs"), 3));
        }
        let mut pr = snapshot();
        pr.body = "log line\n".repeat(40_000);
        let config = config(vec![question("q", true)]);

        let state = build_state(&config, &pr, Some(&diff)).unwrap();

        assert!(tokens(&request(&config, &state)) < TOKEN_CAP);
        assert!(state.condensed.unwrap().contains("long texts cut short"));
        let files = state.value["files"].as_array().unwrap();
        assert!(
            files
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("more files")
        );
    }

    #[test]
    fn every_request_asks_for_zero_data_retention_from_typesafe_only() {
        let config = Config {
            evidence: vec![Evidence::LinkedIssue, Evidence::Diff],
            ..config(vec![Question {
                true_when: Some("The diff does what the issue asks".into()),
                ..question("resolves-issue", true)
            }])
        };
        let mut pr = snapshot();
        pr.linked_issues = vec![issue()];
        let state = build_state(&config, &pr, Some("diff --git a/x b/x\n")).unwrap();

        let body = request(&config, &state);

        assert_eq!(body["model"], "typesafe-ai/jev");
        assert_eq!(
            body["providerOptions"],
            json!({ "gateway": { "zeroDataRetention": true, "only": ["typesafe-ai"] } })
        );
        let asked = &body["questions"]["resolves-issue"];
        assert_eq!(asked["type"], "boolean");
        assert_eq!(asked["instructions"]["question"], "Is resolves-issue so?");
        let mut inspect: Vec<&str> = asked["instructions"]["inspect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field.as_str().unwrap())
            .collect();
        inspect.sort_unstable();
        assert_eq!(inspect, ["diff", "linked_issues", "title"]);
        assert_eq!(
            asked["criteria"],
            json!({ "true": "The diff does what the issue asks" })
        );
        assert_eq!(body["state"]["linked_issues"][0]["issue"], "o/r#3");
        assert!(body["state"].get("description").is_none());
    }

    fn reply(answers: &[(&str, f64)]) -> Reply {
        Reply {
            answers: answers
                .iter()
                .map(|(id, p)| ((*id).to_owned(), *p))
                .collect(),
            warnings: vec![],
            usage: Usage {
                model: MODEL.into(),
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 0,
                usd: Some(0.0001),
            },
            generation: None,
        }
    }

    #[test]
    fn every_question_must_pass_and_a_wrong_answer_fails_with_a_finding() {
        let config = config(vec![question("does", true), question("unmentioned", false)]);

        let pass = judge(
            &config,
            &reply(&[("does", 0.93), ("unmentioned", 0.1)]),
            None,
        );
        assert_eq!(pass.verdict, Verdict::Pass);
        assert!(pass.outputs.findings.is_empty());
        assert_eq!(
            pass.outputs.note.as_deref(),
            Some("2 of 2 questions passed")
        );
        assert_eq!(pass.outputs.probability, Some(0.9));

        let fail = judge(
            &config,
            &reply(&[("does", 0.93), ("unmentioned", 0.88)]),
            Some("diff condensed"),
        );
        assert_eq!(fail.verdict, Verdict::Fail);
        assert_eq!(fail.outputs.findings.len(), 1);
        let finding = &fail.outputs.findings[0];
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(
            finding.message,
            "Is unmentioned so? Jev answered yes (88% sure), and it passes on no, so \
             `unmentioned` fails."
        );
        assert_eq!(
            fail.outputs.note.as_deref(),
            Some("1 of 2 questions failed; diff condensed")
        );
    }

    #[test]
    fn an_unsure_answer_or_a_warning_is_inconclusive() {
        let config = config(vec![question("does", true)]);

        let unsure = judge(&config, &reply(&[("does", 0.55)]), None);
        assert_eq!(unsure.verdict, Verdict::Inconclusive);
        assert_eq!(unsure.outputs.findings[0].severity, Severity::Warning);

        let mut warned = reply(&[("does", 0.01)]);
        warned.warnings = vec!["state truncated".into()];
        let warned = judge(&config, &warned, None);
        assert_eq!(warned.verdict, Verdict::Inconclusive);
        assert_eq!(warned.outputs.findings.len(), 1, "the answer doesn't count");
        assert_eq!(
            warned.outputs.note.as_deref(),
            Some("Jev warned about its answers")
        );
        assert!(
            warned.outputs.findings[0]
                .message
                .contains("state truncated")
        );
    }

    #[test]
    fn an_answer_reads_only_when_it_answers_every_question_and_nothing_else() {
        let config = config(vec![question("a", true)]);
        let good = json!({
            "answers": { "a": { "type": "boolean", "probability": 0.75 } },
            "usage": { "inputTokens": 303, "outputTokens": 20 },
            "providerMetadata": { "gateway": { "cost": "0.0000127", "generationId": "gen_1" } },
        });

        let read = Reply::read(&config, &good).unwrap();

        assert_eq!(read.answers["a"], 0.75);
        assert_eq!(read.usage.input_tokens, 303);
        assert_eq!(read.usage.usd, Some(0.0000127));
        assert_eq!(read.generation.as_deref(), Some("gen_1"));
        for bad in [
            json!({ "answers": {} }),
            json!({ "answers": { "a": { "probability": 1.5 } } }),
            json!({ "answers": { "a": { "probability": 0.5 }, "b": { "probability": 0.5 } } }),
            json!({}),
        ] {
            assert!(Reply::read(&config, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_config_needs_questions_with_distinct_ids_and_a_sane_confidence() {
        let read = |with: Value| -> Result<Config, String> {
            let config: Config = serde_json::from_value(with).map_err(|e| e.to_string())?;
            config.check().map(|()| config)
        };

        let config = read(json!({
            "questions": [{ "id": "a", "question": "A?", "pass_when": true }],
        }))
        .unwrap();
        assert_eq!(config.evidence, [Evidence::Description, Evidence::Diff]);
        assert!(read(json!({ "questions": [] })).is_err());
        assert!(read(json!({ "questions": [{ "id": "a", "question": "A?" }] })).is_err());
        assert!(
            read(json!({
                "questions": [
                    { "id": "a", "question": "A?", "pass_when": true },
                    { "id": "a", "question": "B?", "pass_when": true },
                ],
            }))
            .is_err()
        );
        assert!(
            read(json!({
                "confidence": 0.4,
                "questions": [{ "id": "a", "question": "A?", "pass_when": true }],
            }))
            .is_err()
        );
        assert!(read(json!({ "questions": [], "model": "x" })).is_err());
    }

    /// Resolves `lib/` Steps to the shipped presets.
    struct Presets;

    impl Resolver for Presets {
        fn library_step(&self, name: &str) -> Option<String> {
            slopwatch_core::PRESETS
                .iter()
                .find(|preset| preset.name == name)
                .map(|preset| preset.text.to_owned())
        }

        fn plugin(&self, name: &str) -> Option<PluginInfo> {
            (name == "jev").then_some(PluginInfo {
                workspace: Workspace::None,
                builtin: true,
            })
        }
    }

    #[test]
    fn both_presets_are_configs_jev_reads() {
        let pipeline = slopwatch_core::load(
            "version: 1\nsteps:\n  desc: { uses: lib/desc-matches-diff }\n  issue: { uses: lib/resolves-issue, when: { linked_issue: true } }\ngate: [desc, { issue: [pass, skipped] }]\n",
            &Presets,
        )
        .unwrap();

        for id in ["desc", "issue"] {
            let step = pipeline.step(id).unwrap();
            assert_eq!(step.plugin, "jev");
            let config: Config = serde_json::from_value(Value::Object(step.config.clone()))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
            config.check().unwrap();
        }
        let issue: Config = serde_json::from_value(Value::Object(
            pipeline.step("issue").unwrap().config.clone(),
        ))
        .unwrap();
        assert!(issue.wants(Evidence::LinkedIssue));
    }

    /// A fake AI Gateway: answers each request with the next scripted
    /// response and keeps what it was sent.
    struct FakeGateway {
        url: String,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl FakeGateway {
        fn start(responses: Vec<(u16, Value)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/v1/evaluate", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            std::thread::spawn(move || {
                for (status, body) in responses {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let (auth, sent) = read_request(&mut stream);
                    seen.lock().unwrap().push((auth, sent));
                    let text = body.to_string();
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                }
            });
            Self { url, requests }
        }

        fn gateway(&self) -> impl FnOnce(String) -> Gateway + use<> {
            let url = self.url.clone();
            move |key| Gateway::new(url, key, vec![Duration::from_millis(10); 3])
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// One HTTP request's `authorization` header and JSON body.
    fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
        let mut buffer = Vec::new();
        let mut chunk = [0; 8192];
        let head = loop {
            let n = stream.read(&mut chunk).unwrap();
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..head]).into_owned();
        let header = |name: &str| {
            headers.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let length: usize = header("content-length").map_or(0, |v| v.parse().unwrap());
        while buffer.len() < head + length {
            let n = stream.read(&mut chunk).unwrap();
            buffer.extend_from_slice(&chunk[..n]);
        }
        let body = serde_json::from_slice(&buffer[head..head + length]).unwrap();
        (header("authorization").unwrap_or_default(), body)
    }

    fn answered(probability: f64) -> (u16, Value) {
        (
            200,
            json!({
                "answers": { "does": { "type": "boolean", "probability": probability } },
                "usage": { "inputTokens": 512, "outputTokens": 10 },
                "providerMetadata": { "gateway": { "cost": "0.0000215", "generationId": "gen_x" } },
            }),
        )
    }

    /// Runs a session with `with:` and the snapshot, its diff in a temp
    /// file, and returns what the Step said, or the error it exited with.
    fn start_session(
        with: Value,
        mut pr: PrSnapshot,
        diff: &str,
        fake: &FakeGateway,
    ) -> Result<Vec<FromStep>, std::io::Error> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr.diff");
        std::fs::write(&path, diff).unwrap();
        pr.diff = Some(path);
        let start = ToStep::Start(Start {
            run: slopwatch_protocol::RunId(1),
            step: "desc".into(),
            config: with.as_object().unwrap().clone(),
            snapshot: pr,
            upstream: BTreeMap::new(),
            gate_failing: vec![],
            ci_logs: vec![],
            budget_usd: None,
        });
        let input = format!("{}\n", serde_json::to_string(&start).unwrap());
        let mut output = Vec::new();
        session(
            std::io::Cursor::new(input),
            &mut output,
            Some("test-gateway-key".into()),
            fake.gateway(),
        )?;
        Ok(String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect())
    }

    fn one_question() -> Value {
        json!({ "questions": [{ "id": "does", "question": "Does it?", "pass_when": true }] })
    }

    #[test]
    fn a_session_asks_once_and_reports_usage_and_the_outcome() {
        let fake = FakeGateway::start(vec![answered(0.96)]);

        let said = start_session(
            one_question(),
            snapshot(),
            "diff --git a/x b/x\n+y\n",
            &fake,
        )
        .unwrap();

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let (auth, body) = &requests[0];
        assert_eq!(auth, "Bearer test-gateway-key");
        assert_eq!(
            body["providerOptions"]["gateway"]["zeroDataRetention"],
            true
        );
        assert_eq!(body["state"]["diff"], "diff --git a/x b/x\n+y\n");
        assert!(matches!(said[0], FromStep::Progress { .. }));
        assert_eq!(
            said[1],
            FromStep::Usage(Usage {
                model: MODEL.into(),
                input_tokens: 512,
                cached_input_tokens: 0,
                output_tokens: 10,
                usd: Some(0.0000215),
            })
        );
        let FromStep::Outcome(outcome) = &said[2] else {
            panic!("expected an Outcome, got {said:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_rate_limit_is_retried_after_a_wait() {
        let fake = FakeGateway::start(vec![
            (429, json!({ "error": "at capacity" })),
            (503, json!({ "error": "unavailable" })),
            answered(0.1),
        ]);

        let said =
            start_session(one_question(), snapshot(), "diff --git a/x b/x\n", &fake).unwrap();

        assert_eq!(fake.requests().len(), 3);
        let Some(FromStep::Outcome(outcome)) = said.last() else {
            panic!("expected an Outcome, got {said:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(outcome.outputs.findings.len(), 1);
    }

    #[test]
    fn a_request_refused_for_zero_data_retention_errors_without_a_retry() {
        let fake = FakeGateway::start(vec![
            (
                400,
                json!({ "error": { "type": "no_zdr_providers_available", "message": "No ZDR" } }),
            ),
            answered(0.9),
        ]);

        let error =
            start_session(one_question(), snapshot(), "diff --git a/x b/x\n", &fake).unwrap_err();

        assert!(error.to_string().contains("zero-data-retention"), "{error}");
        let requests = fake.requests();
        assert_eq!(requests.len(), 1, "never sent again");
        assert_eq!(
            requests[0].1["providerOptions"]["gateway"],
            json!({ "zeroDataRetention": true, "only": ["typesafe-ai"] })
        );
    }

    #[test]
    fn a_bad_key_errors_without_a_retry_and_retries_run_out() {
        let fake = FakeGateway::start(vec![(401, json!({ "error": "bad key" }))]);
        let error = start_session(one_question(), snapshot(), "", &fake).unwrap_err();
        assert!(error.to_string().contains("401"), "{error}");
        assert_eq!(fake.requests().len(), 1);

        let busy = (429, json!({ "error": "at capacity" }));
        let fake = FakeGateway::start(vec![busy.clone(), busy.clone(), busy.clone(), busy]);
        let error = start_session(one_question(), snapshot(), "", &fake).unwrap_err();
        assert!(error.to_string().contains("didn't answer"), "{error}");
        assert_eq!(fake.requests().len(), 4, "the first try and three retries");
    }

    #[test]
    fn a_pr_without_a_linked_issue_is_inconclusive_without_asking() {
        let fake = FakeGateway::start(vec![]);
        let with = json!({
            "evidence": ["linked_issue", "diff"],
            "questions": [{ "id": "does", "question": "Does it?", "pass_when": true }],
        });

        let said = start_session(with.clone(), snapshot(), "", &fake).unwrap();

        let FromStep::Outcome(outcome) = &said[0] else {
            panic!("expected an Outcome, got {said:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert!(
            outcome.outputs.findings[0]
                .message
                .contains("linked_issue: true")
        );
        assert!(fake.requests().is_empty());

        let fake = FakeGateway::start(vec![answered(0.9)]);
        let mut pr = snapshot();
        pr.linked_issues = vec![issue()];
        start_session(with, pr, "", &fake).unwrap();
        assert_eq!(
            fake.requests()[0].1["state"]["linked_issues"][0]["title"],
            "Parser crashes on empty input"
        );
    }

    #[test]
    fn an_unreadable_config_fails_the_step() {
        let fake = FakeGateway::start(vec![]);

        let said = start_session(json!({ "questions": [] }), snapshot(), "", &fake).unwrap();

        let FromStep::Outcome(outcome) = &said[0] else {
            panic!("expected an Outcome, got {said:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Fail);
        assert!(fake.requests().is_empty());
    }
}
