//! Graph rules a Pipeline must satisfy to load (ADR 0006).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::error::{LoadError, Referrer};
use crate::expr::Expr;
use crate::pipeline::{GATE, Step};

/// Checks the graph and returns every node (Step ids plus [`GATE`]) in an
/// order where each comes after everything it reads.
pub(crate) fn graph(
    steps: &BTreeMap<String, Step>,
    gate: &[Expr],
) -> Result<Vec<String>, Vec<LoadError>> {
    let gate_refs = step_refs(gate.iter());
    let known = |id: &str| steps.contains_key(id);

    let mut errors = Vec::new();
    for step in steps.values() {
        for target in &step.needs {
            if target != GATE && !known(target) {
                errors.push(LoadError::UnknownNeed {
                    step: step.id.clone(),
                    target: target.clone(),
                });
            }
        }
        if let Some(when) = &step.when {
            for target in step_refs([when]) {
                if !known(target) {
                    errors.push(LoadError::UnknownReference {
                        referrer: Referrer::Condition(step.id.clone()),
                        target: target.to_owned(),
                    });
                }
            }
        }
    }
    for target in &gate_refs {
        if !known(target) {
            errors.push(LoadError::UnknownReference {
                referrer: Referrer::Gate,
                target: (*target).to_owned(),
            });
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let inputs = |node: &str| -> Vec<&str> {
        if node == GATE {
            gate_refs.iter().copied().collect()
        } else {
            steps[node].needs.iter().map(String::as_str).collect()
        }
    };
    let nodes: Vec<&str> = steps.keys().map(String::as_str).chain([GATE]).collect();
    let order = topological(&nodes, &inputs).map_err(|cycle| vec![LoadError::Cycle(cycle)])?;

    let mut ancestors: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for &node in &order {
        let mut set = BTreeSet::new();
        for input in inputs(node) {
            set.insert(input);
            set.extend(ancestors[input].iter().copied());
        }
        ancestors.insert(node, set);
    }

    let is_write = |id: &str| steps.get(id).is_some_and(Step::is_write);
    for step in steps.values() {
        for target in &step.needs {
            if is_write(target) {
                errors.push(LoadError::NeedsWriteStep {
                    step: step.id.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    for target in &gate_refs {
        if is_write(target) {
            errors.push(LoadError::ReferencesWriteStep {
                referrer: Referrer::Gate,
                target: (*target).to_owned(),
            });
        }
    }
    for step in steps.values() {
        let Some(when) = &step.when else { continue };
        let upstream = &ancestors[step.id.as_str()];
        let mut targets = step_refs([when]);
        if reads_gate(when) {
            targets.insert(GATE);
        }
        for target in targets {
            if is_write(target) {
                errors.push(LoadError::ReferencesWriteStep {
                    referrer: Referrer::Condition(step.id.clone()),
                    target: target.to_owned(),
                });
            } else if !upstream.contains(target) {
                errors.push(LoadError::NotUpstream {
                    step: step.id.clone(),
                    target: target.to_owned(),
                });
            }
        }
    }
    for step in steps.values().filter(|s| s.is_merge()) {
        if ancestors[GATE].contains(step.id.as_str()) {
            errors.push(LoadError::MergeBeforeGate(step.id.clone()));
        } else if !ancestors[step.id.as_str()].contains(GATE) {
            errors.push(LoadError::MergeNotAfterGate(step.id.clone()));
        }
    }

    if errors.is_empty() {
        Ok(order.into_iter().map(str::to_owned).collect())
    } else {
        Err(errors)
    }
}

fn step_refs<'a>(exprs: impl IntoIterator<Item = &'a Expr>) -> BTreeSet<&'a str> {
    let mut refs = BTreeSet::new();
    for expr in exprs {
        expr.walk(&mut |e| {
            if let Expr::Step(term) = e {
                refs.insert(term.id.as_str());
            }
        });
    }
    refs
}

fn reads_gate(expr: &Expr) -> bool {
    let mut found = false;
    expr.walk(&mut |e| found |= matches!(e, Expr::Gate));
    found
}

/// Orders `nodes` so each comes after its inputs, or returns a cycle as the
/// path of ids from a node back to itself.
fn topological<'a>(
    nodes: &[&'a str],
    inputs: &impl Fn(&str) -> Vec<&'a str>,
) -> Result<Vec<&'a str>, Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit<'a>(
        node: &'a str,
        inputs: &impl Fn(&str) -> Vec<&'a str>,
        marks: &mut HashMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
        order: &mut Vec<&'a str>,
    ) -> Result<(), Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                let start = path.iter().position(|&n| n == node).unwrap_or(0);
                let mut cycle: Vec<String> = path[start..].iter().map(|n| n.to_string()).collect();
                cycle.push(node.to_owned());
                return Err(cycle);
            }
            None => {}
        }
        marks.insert(node, Mark::Visiting);
        path.push(node);
        for input in inputs(node) {
            visit(input, inputs, marks, path, order)?;
        }
        path.pop();
        marks.insert(node, Mark::Done);
        order.push(node);
        Ok(())
    }

    let mut marks = HashMap::new();
    let mut order = Vec::new();
    for &node in nodes {
        visit(node, inputs, &mut marks, &mut Vec::new(), &mut order)?;
    }
    Ok(order)
}
