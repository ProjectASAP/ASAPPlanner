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
- `Pass2Candidate { .. }` and `Pass2Of { .. }`, which carry nearly the same fields
  as ready-formatted strings, plus `SharedSummary { group: usize, members:
  Vec<usize> }`, which indexes into two other lists → one
  `Stage1Candidate.pass2: Option<Pass2Candidate>` holding the Pass 1 candidate
  and the shared build nodes themselves (`shared_builds:
  Vec<Vec<Rc<OperatorNode>>>`), with display text moved to the devtools.
  Trade-off: one more rename across the Pass 2 rules, but readers no longer
  follow indexes.

The author picks; the reviewer does not decide.

Required step: review the implementation's performance complexity
(efficiency) as well. For the new or changed code paths, check:

- Time and space complexity: quadratic or exponential loops, subset
  enumeration, recomputation inside loops (for example a reachability walk
  per node), unnecessary clones of DAGs or roots, and eager enumeration that
  could be lazy or capped.
- Allocations on hot paths.
- How the cost scales with the number of queries, candidates and series.

Each efficiency finding states the complexity, a realistic input size at which
it hurts, and a concrete cheaper alternative. When the cheaper alternative has
a trade-off, give the author choices here too.

For example:

- `price` recomputes `query_reach(dag)`, a walk over the whole DAG, for every
  MatAtQuery node: O(n · (n + e)) per candidate, times every candidate. At
  Example 3's 972 candidates, if each DAG has about 50 nodes, that is about 2.5·10⁶
  node visits instead of 5·10⁴. Alternative: compute it once per candidate
  before the loop and pass it in.
- Subset enumeration over one sharing group's members is 2^n. It is capped by
  `MAX_GROUP_MEMBERS = 20` (about 10⁶ subsets). Choices: keep the cap and
  report a group that hits it, or enumerate lazily and stop at
  `--max-combinations`.

Review output format, in this order:

1. **Findings**: correctness, regressions and other defects, by severity.
2. **Interface simplifications (author's choice)**: the options above.
3. **Efficiency**: complexity, the input size where it hurts, the
   alternative, and choices where there is a trade-off.

Write "None" under a section with nothing to report.

Check that tests exercise the changed behavior, including relevant unit and
end-to-end paths. For a bug fix, verify that a regression test would fail without
the production change.

For materially affected quality attributes—maintainability, debuggability,
scalability, understandability, extensibility, performance, and operability—ask
for measurable evidence where practical. Do not invent numerical precision.

Report findings first (section 1 above), ordered by severity, with precise
file and line references. State concrete impact and a viable correction. Keep summaries brief;
if no actionable findings remain, say so and identify any material testing gap.
