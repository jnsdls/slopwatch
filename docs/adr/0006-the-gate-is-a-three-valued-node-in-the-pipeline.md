# The Gate is a three-valued node in the Pipeline

The Gate reads only Verdicts, never outputs such as a probability, because each Step already applies its own threshold. It evaluates to pass, fail or pending with short-circuit logic, so `or: [a, b]` passes once `a` passes even while `b` runs. It also sits in the graph as a node, so Steps can run after it: Merge with the Condition `gate`, and Fix with `not: [gate]`. We considered evaluating the Gate only after every Step settled, and keeping Fix upstream of the Gate with a Condition over individual failures. We dropped both. The first holds Merge back behind Steps that can no longer change the answer. The second lets Fix start while the Gate is still waiting on a Human Step or slow CI, which risks a push that ends a Run the developer was about to approve.

## Consequences

- A determined Gate never cancels running Steps. A fail still waits for Fix, and a pass lets Merge start, with leftovers cancelled once it merges. Without Merge, "shippable" is announced when the Gate passes, and the Run ends when every Step has settled.
- A Step the Gate doesn't reference is advisory. That is how a new judgement starts, so there's no per-Step off/warning/error mode.
- The Gate and Conditions can't reference a Step that declares `workspace: write`, and a Condition can only reference Steps upstream of it. The daemon rejects a Pipeline that breaks either rule when it loads it.
