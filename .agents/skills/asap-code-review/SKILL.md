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

Check that tests exercise the changed behavior, including relevant unit and
end-to-end paths. For a bug fix, verify that a regression test would fail without
the production change.

For materially affected quality attributes—maintainability, debuggability,
scalability, understandability, extensibility, performance, and operability—ask
for measurable evidence where practical. Do not invent numerical precision.

Report findings first, ordered by severity, with precise file and line
references. State concrete impact and a viable correction. Keep summaries brief;
if no actionable findings remain, say so and identify any material testing gap.
