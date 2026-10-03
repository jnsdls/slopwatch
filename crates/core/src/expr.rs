//! The expression language shared by the Gate and Conditions: Mergify's
//! shape, where a list means AND, with `or:`, `and:` and `not:` blocks.
//! Evaluation is three-valued (Kleene), so an expression is determined as
//! soon as its known parts decide it.

use std::fmt;

use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;

use crate::run::PrFacts;
use crate::verdict::{GateState, StepState, Verdict};

/// Words a Step id can't take, because the expression language or the Gate
/// node already uses them.
pub(crate) const RESERVED_IDS: &[&str] = &[
    "gate", "always", "and", "or", "not", "files", "labels", "base", "draft", "author",
];

#[derive(Debug, Clone)]
pub enum Expr {
    /// `always` (or `true`) and `false`.
    Const(bool),
    Step(StepTerm),
    /// The Gate, readable from a Condition downstream of it.
    Gate,
    Fact(Fact),
    /// A list or an `and:` block.
    All(Vec<Expr>),
    /// An `or:` block.
    Any(Vec<Expr>),
    /// A `not:` block: true when the AND of its items is false.
    Not(Vec<Expr>),
}

/// A reference to a Step's Verdict. Only `pass` satisfies it, unless it also
/// accepts `skipped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTerm {
    pub id: String,
    pub accepts_skipped: bool,
}

/// A fact about the PR, taken from the head-SHA snapshot. A list matches
/// when any of its values does.
#[derive(Debug, Clone)]
pub enum Fact {
    /// Any changed file matches any of the globs.
    Files(Vec<Glob>),
    /// The PR carries any of the labels.
    Labels(Vec<String>),
    Base(Vec<String>),
    Author(Vec<String>),
    Draft(bool),
}

#[derive(Debug, Clone)]
pub struct Glob {
    pattern: String,
    matcher: GlobMatcher,
}

impl Glob {
    fn new(pattern: &str) -> Result<Glob, String> {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("invalid glob `{pattern}`: {}", e.kind()))?;
        Ok(Glob {
            pattern: pattern.to_owned(),
            matcher: glob.compile_matcher(),
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn is_match(&self, path: &str) -> bool {
        self.matcher.is_match(path)
    }
}

/// Which part of the file an expression comes from. The Gate reads only
/// Verdicts; a Condition can also read the Gate and PR facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    Gate,
    Condition,
}

/// Three-valued truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

/// What an expression reads while it evaluates.
pub(crate) trait Env {
    /// The Step's state, with a Waiver already counted as pass.
    fn step_state(&self, id: &str) -> StepState;
    fn gate(&self) -> GateState;
    fn pr(&self) -> &PrFacts;
}

impl Expr {
    pub(crate) fn parse(value: &Value, cx: Context) -> Result<Expr, String> {
        match value {
            Value::String(s) => match s.as_str() {
                "always" => condition_only(cx, "always", Expr::Const(true)),
                "gate" => condition_only(cx, "gate", Expr::Gate),
                id => Ok(Expr::Step(StepTerm {
                    id: id.to_owned(),
                    accepts_skipped: false,
                })),
            },
            Value::Bool(b) => condition_only(cx, &b.to_string(), Expr::Const(*b)),
            Value::Array(items) => Ok(Expr::All(parse_list(items, cx)?)),
            Value::Object(map) => {
                let mut entries = map.iter();
                let (Some((key, inner)), None) = (entries.next(), entries.next()) else {
                    let keys: Vec<_> = map.keys().map(|k| format!("`{k}`")).collect();
                    return Err(format!(
                        "a term is a map with exactly one key, found {}; use a list for AND",
                        if keys.is_empty() {
                            "none".to_owned()
                        } else {
                            keys.join(", ")
                        }
                    ));
                };
                match key.as_str() {
                    "and" => Ok(Expr::All(parse_block(key, inner, cx)?)),
                    "or" => Ok(Expr::Any(parse_block(key, inner, cx)?)),
                    "not" => Ok(Expr::Not(parse_block(key, inner, cx)?)),
                    "files" | "labels" | "base" | "author" | "draft" => {
                        if cx == Context::Gate {
                            return Err(format!(
                                "the Gate reads only Verdicts, so it can't read the PR fact `{key}`"
                            ));
                        }
                        parse_fact(key, inner).map(Expr::Fact)
                    }
                    "gate" | "always" => Err(format!("`{key}` takes no value")),
                    id => parse_step_term(id, inner),
                }
            }
            other => Err(format!(
                "expected a Step id, a list or a one-key map, found {}",
                describe(other)
            )),
        }
    }

    pub(crate) fn eval(&self, env: &dyn Env) -> Tri {
        match self {
            Expr::Const(true) => Tri::True,
            Expr::Const(false) => Tri::False,
            Expr::Step(term) => term.eval(env.step_state(&term.id)),
            Expr::Gate => match env.gate() {
                GateState::Pass => Tri::True,
                GateState::Fail => Tri::False,
                GateState::Pending => Tri::Unknown,
            },
            Expr::Fact(fact) => {
                if fact.holds(env.pr()) {
                    Tri::True
                } else {
                    Tri::False
                }
            }
            Expr::All(items) => all(items, env),
            Expr::Any(items) => {
                let mut result = Tri::False;
                for item in items {
                    match item.eval(env) {
                        Tri::True => return Tri::True,
                        Tri::Unknown => result = Tri::Unknown,
                        Tri::False => {}
                    }
                }
                result
            }
            Expr::Not(items) => all(items, env).not(),
        }
    }

    /// Calls `f` on this expression and every expression nested in it.
    pub(crate) fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        f(self);
        if let Expr::All(items) | Expr::Any(items) | Expr::Not(items) = self {
            for item in items {
                item.walk(f);
            }
        }
    }
}

pub(crate) fn all(items: &[Expr], env: &dyn Env) -> Tri {
    let mut result = Tri::True;
    for item in items {
        match item.eval(env) {
            Tri::False => return Tri::False,
            Tri::Unknown => result = Tri::Unknown,
            Tri::True => {}
        }
    }
    result
}

impl StepTerm {
    pub(crate) fn eval(&self, state: StepState) -> Tri {
        match state {
            StepState::Pending | StepState::Running => Tri::Unknown,
            StepState::Settled(Verdict::Pass) => Tri::True,
            StepState::Settled(Verdict::Skipped) if self.accepts_skipped => Tri::True,
            StepState::Settled(_) => Tri::False,
        }
    }
}

impl Fact {
    fn holds(&self, pr: &PrFacts) -> bool {
        match self {
            Fact::Files(globs) => pr
                .files
                .iter()
                .any(|file| globs.iter().any(|g| g.is_match(file))),
            Fact::Labels(labels) => labels.iter().any(|l| pr.labels.contains(l)),
            Fact::Base(bases) => bases.contains(&pr.base),
            Fact::Author(authors) => authors.contains(&pr.author),
            Fact::Draft(draft) => pr.draft == *draft,
        }
    }
}

fn condition_only(cx: Context, word: &str, expr: Expr) -> Result<Expr, String> {
    match cx {
        Context::Condition => Ok(expr),
        Context::Gate => Err(format!(
            "`{word}` can only appear in a Condition, not the Gate"
        )),
    }
}

fn parse_list(items: &[Value], cx: Context) -> Result<Vec<Expr>, String> {
    if items.is_empty() {
        return Err("an empty list has no terms".to_owned());
    }
    items.iter().map(|item| Expr::parse(item, cx)).collect()
}

fn parse_block(key: &str, inner: &Value, cx: Context) -> Result<Vec<Expr>, String> {
    match inner {
        Value::Array(items) if items.is_empty() => Err(format!("`{key}` needs at least one term")),
        Value::Array(items) => parse_list(items, cx),
        _ => Err(format!("`{key}` takes a list of terms")),
    }
}

fn parse_fact(key: &str, inner: &Value) -> Result<Fact, String> {
    if key == "draft" {
        return match inner {
            Value::Bool(b) => Ok(Fact::Draft(*b)),
            other => Err(format!(
                "`draft` takes true or false, found {}",
                describe(other)
            )),
        };
    }
    let values = strings(inner).ok_or_else(|| {
        format!(
            "`{key}` takes a string or a list of strings, found {}",
            describe(inner)
        )
    })?;
    Ok(match key {
        "files" => Fact::Files(
            values
                .iter()
                .map(|p| Glob::new(p))
                .collect::<Result<_, _>>()?,
        ),
        "labels" => Fact::Labels(values),
        "base" => Fact::Base(values),
        "author" => Fact::Author(values),
        _ => unreachable!("parse_fact called with `{key}`"),
    })
}

fn parse_step_term(id: &str, inner: &Value) -> Result<Expr, String> {
    let names = strings(inner)
        .filter(|names| !names.is_empty())
        .ok_or_else(|| {
            format!("`{id}` takes a list of accepted Verdicts, such as [pass, skipped]")
        })?;
    let mut accepts_skipped = false;
    for name in &names {
        match Verdict::parse(name) {
            Some(Verdict::Pass) => {}
            Some(Verdict::Skipped) => accepts_skipped = true,
            Some(v) => {
                return Err(format!("`{id}` can accept only pass and skipped, not {v}"));
            }
            None => return Err(format!("`{id}` lists `{name}`, which isn't a Verdict")),
        }
    }
    Ok(Expr::Step(StepTerm {
        id: id.to_owned(),
        accepts_skipped,
    }))
}

fn strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::Null => "nothing".to_owned(),
        Value::Bool(b) => format!("`{b}`"),
        Value::Number(n) => format!("the number {n}"),
        Value::String(s) => format!("`{s}`"),
        Value::Array(_) => "a list".to_owned(),
        Value::Object(_) => "a map".to_owned(),
    }
}

/// Renders an expression in YAML flow style, for skip reasons and errors.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn list(f: &mut fmt::Formatter<'_>, items: &[impl fmt::Display]) -> fmt::Result {
            f.write_str("[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{item}")?;
            }
            f.write_str("]")
        }
        match self {
            Expr::Const(true) => f.write_str("always"),
            Expr::Const(false) => f.write_str("false"),
            Expr::Step(StepTerm {
                id,
                accepts_skipped: false,
            }) => f.write_str(id),
            Expr::Step(StepTerm {
                id,
                accepts_skipped: true,
            }) => write!(f, "{{{id}: [pass, skipped]}}"),
            Expr::Gate => f.write_str("gate"),
            Expr::Fact(fact) => match fact {
                Fact::Files(globs) => {
                    let patterns: Vec<_> = globs.iter().map(Glob::pattern).collect();
                    f.write_str("{files: ")?;
                    list(f, &patterns)?;
                    f.write_str("}")
                }
                Fact::Labels(v) | Fact::Base(v) | Fact::Author(v) => {
                    let key = match fact {
                        Fact::Labels(_) => "labels",
                        Fact::Base(_) => "base",
                        _ => "author",
                    };
                    write!(f, "{{{key}: ")?;
                    list(f, v)?;
                    f.write_str("}")
                }
                Fact::Draft(b) => write!(f, "{{draft: {b}}}"),
            },
            Expr::All(items) => list(f, items),
            Expr::Any(items) => {
                f.write_str("{or: ")?;
                list(f, items)?;
                f.write_str("}")
            }
            Expr::Not(items) => {
                f.write_str("{not: ")?;
                list(f, items)?;
                f.write_str("}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    struct TestEnv {
        steps: HashMap<&'static str, StepState>,
        gate: GateState,
        pr: PrFacts,
    }

    impl TestEnv {
        fn new(steps: &[(&'static str, StepState)]) -> TestEnv {
            TestEnv {
                steps: steps.iter().copied().collect(),
                gate: GateState::Pending,
                pr: PrFacts::default(),
            }
        }
    }

    impl Env for TestEnv {
        fn step_state(&self, id: &str) -> StepState {
            self.steps.get(id).copied().unwrap_or_default()
        }
        fn gate(&self) -> GateState {
            self.gate
        }
        fn pr(&self) -> &PrFacts {
            &self.pr
        }
    }

    fn gate(v: Value) -> Expr {
        Expr::parse(&v, Context::Gate).unwrap()
    }

    fn cond(v: Value) -> Expr {
        Expr::parse(&v, Context::Condition).unwrap()
    }

    const PASS: StepState = StepState::Settled(Verdict::Pass);
    const FAIL: StepState = StepState::Settled(Verdict::Fail);
    const SKIPPED: StepState = StepState::Settled(Verdict::Skipped);

    #[test]
    fn or_passes_once_one_side_passes_while_the_other_is_pending() {
        let expr = gate(json!([{"or": ["a", "b"]}]));
        let env = TestEnv::new(&[("a", PASS), ("b", StepState::Running)]);
        assert_eq!(expr.eval(&env), Tri::True);
    }

    #[test]
    fn or_fails_only_once_both_sides_fail() {
        let expr = gate(json!([{"or": ["a", "b"]}]));
        assert_eq!(expr.eval(&TestEnv::new(&[("a", FAIL)])), Tri::Unknown);
        assert_eq!(
            expr.eval(&TestEnv::new(&[("a", FAIL), ("b", FAIL)])),
            Tri::False
        );
    }

    #[test]
    fn and_fails_as_soon_as_one_term_fails() {
        let expr = gate(json!(["a", "b"]));
        assert_eq!(expr.eval(&TestEnv::new(&[("a", FAIL)])), Tri::False);
        assert_eq!(expr.eval(&TestEnv::new(&[("a", PASS)])), Tri::Unknown);
        assert_eq!(
            expr.eval(&TestEnv::new(&[("a", PASS), ("b", PASS)])),
            Tri::True
        );
    }

    #[test]
    fn not_stays_pending_until_its_operand_is_determined() {
        let expr = gate(json!([{"not": ["a"]}]));
        assert_eq!(expr.eval(&TestEnv::new(&[])), Tri::Unknown);
        assert_eq!(expr.eval(&TestEnv::new(&[("a", FAIL)])), Tri::True);
    }

    #[test]
    fn a_term_is_satisfied_only_by_pass_unless_it_accepts_skipped() {
        let strict = gate(json!(["docs"]));
        let lenient = gate(json!([{"docs": ["pass", "skipped"]}]));
        let env = TestEnv::new(&[("docs", SKIPPED)]);
        assert_eq!(strict.eval(&env), Tri::False);
        assert_eq!(lenient.eval(&env), Tri::True);
        for verdict in [Verdict::Inconclusive, Verdict::Error, Verdict::Missing] {
            let env = TestEnv::new(&[("docs", StepState::Settled(verdict))]);
            assert_eq!(lenient.eval(&env), Tri::False, "{verdict}");
        }
    }

    #[test]
    fn a_term_can_accept_only_skipped_besides_pass() {
        let err = Expr::parse(&json!({"docs": ["pass", "error"]}), Context::Gate).unwrap_err();
        assert_eq!(err, "`docs` can accept only pass and skipped, not error");
    }

    #[test]
    fn the_gate_cannot_read_pr_facts_or_itself() {
        for v in [
            json!({"files": "docs/**"}),
            json!("gate"),
            json!("always"),
            json!(true),
        ] {
            assert!(Expr::parse(&v, Context::Gate).is_err(), "{v}");
        }
    }

    #[test]
    fn a_term_map_has_exactly_one_key() {
        let err = Expr::parse(&json!({"a": ["pass"], "b": ["pass"]}), Context::Gate).unwrap_err();
        assert!(err.contains("exactly one key"), "{err}");
    }

    #[test]
    fn conditions_read_pr_facts() {
        let mut env = TestEnv::new(&[]);
        env.pr = PrFacts {
            files: vec!["docs/guide/intro.md".into(), "src/main.rs".into()],
            labels: vec!["slopwatch".into()],
            base: "main".into(),
            draft: false,
            author: "jnsdls".into(),
        };
        let truths = [
            (json!({"files": "docs/**"}), Tri::True),
            (json!({"files": "*.md"}), Tri::False),
            (json!({"files": ["*.toml", "src/*.rs"]}), Tri::True),
            (json!({"labels": "slopwatch"}), Tri::True),
            (json!({"base": ["release", "main"]}), Tri::True),
            (json!({"author": "someone"}), Tri::False),
            (json!({"draft": false}), Tri::True),
            (json!(false), Tri::False),
            (json!("always"), Tri::True),
        ];
        for (v, want) in truths {
            assert_eq!(cond(v.clone()).eval(&env), want, "{v}");
        }
    }

    #[test]
    fn conditions_read_the_gate() {
        let mut env = TestEnv::new(&[]);
        let expr = cond(json!({"not": ["gate"]}));
        assert_eq!(expr.eval(&env), Tri::Unknown);
        env.gate = GateState::Fail;
        assert_eq!(expr.eval(&env), Tri::True);
    }

    #[test]
    fn display_renders_flow_yaml() {
        let expr = cond(
            json!(["ci", {"or": [{"docs": ["skipped"]}, {"not": ["gate"]}]}, {"files": "docs/**"}]),
        );
        assert_eq!(
            expr.to_string(),
            "[ci, {or: [{docs: [pass, skipped]}, {not: [gate]}]}, {files: [docs/**]}]"
        );
    }
}
