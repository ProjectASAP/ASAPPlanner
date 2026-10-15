---
name: asap-code-review
description: Review ProjectASAP changes for correctness, regressions, unnecessary complexity, and maintainability. Use when asked to review a diff, branch, commit, or pull request.
---

# Code review

When practical, perform final review in a fresh session or use a reviewer that
did not implement the change. Give the reviewer the requirement, diff, and
necessary repository context without priming it with the author's conclusions.
State when review was not independent.

Review the actual diff and enough surrounding code to validate its assumptions.
Prioritize findings that can cause incorrect behavior, regressions, security
issues, data loss, or operational failure.

Check whether new conceptual layers, types, or interfaces solve a demonstrated
need. Flag duplicated definitions and avoid recommending architecture whose cost
exceeds the stated requirement.

Required step: for the interfaces in the PR, self-ask "any ways to simplify
or make them more human-readable?", and give the author choices. For every
type, field, function, method, CLI flag or format the PR adds or changes,
check whether it could be simpler or easier for a human to read:

- Names are understandable without their comments, and builder or method names
  match the field names they set or read.
- Near-duplicate types or functions are merged (for example, two structs that
  carry the same fields).
- Reserved or unused fields are removed.
- A struct holding indexes into another list holds the items themselves.
- Counts are integers, not `f64`.
- Display strings live outside planner structs.
- Planner-internal helpers are not methods on public types.
- One function replaces a general function plus a special case of it.

Report these in a section **Interface simplifications (author's choice)**. Give
each option the current signature, the proposed signature, and the trade-off,
for example:

- `equal_shares(target, metric, of, shares)` and `equal_share(target, metric, n)`
  → one `per_input_target(target, metric, approximate_inputs)`. Trade-off: only
  the legacy core uses `shares > 1`; merging needs that core gone first.
- `whole_result_guarantee(guarantee, output_values: Option<f64>)` →
  `guarantee.over_whole_result(values_per_result: Option<u64>)`. Trade-off: a
  method on a public type, but a count that cannot be fractional.

The author picks; the reviewer does not decide. These options do not replace
correctness findings and are listed after them.

Check that tests exercise the changed behavior, including relevant unit and
end-to-end paths. For a bug fix, verify that a regression test would fail without
the production change.

For materially affected quality attributes—maintainability, debuggability,
scalability, understandability, extensibility, performance, and operability—ask
for measurable evidence where practical. Do not invent numerical precision.

Report findings first, ordered by severity, with precise file and line
references. State concrete impact and a viable correction. Keep summaries brief;
if no actionable findings remain, say so and identify any material testing gap.
