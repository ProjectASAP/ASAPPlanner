## Summary

Three lines: the problem, the change, and its effect, with one concrete example
and its numbers (for example, the workload or input and the cost, latency, or
output before and after).

## Review guide

- Position: the base branch, and for a stacked PR its place in the stack.
- Reading order: the files or functions to read first.
- Mechanical parts (renames, moves, regenerated files) versus the parts that
  need careful review, named by file or function.

## Behavior: before → after

One end-to-end example from what the user writes (query, requirements, data
facts, or CLI call) to what they get back, before and after this PR side by
side. Include the user, design, correctness, or MVP need it serves.

## Interfaces

Group by source file, with links to the files at a fixed commit. Under each group,
put one fenced `diff` block holding the real declarations from the diff: types
with every field or variant listed (never collapsed into `// ...`), and
function/method/trait signatures with bodies elided. Start each line with `+`
(added), `-` (removed) or a space (unchanged context); show a changed item as
its `-` old line followed by its `+` new line. Say on each type whether it stays
or is removed, and explain each interface in a short doc comment: what it is, what it is for, and when
it is used or produced. End with an end-to-end usage example: from what the user
writes (query, requirements, data facts, or CLI call), through where this PR's
interfaces are called, to what the user gets back, with every variable commented
and the numbers checked by running it. Write "None" if no interface changes.
Removed interfaces appear as `-` lines; give a one-line reason under the block
for any removal whose reason is not obvious.

## Algorithm changes

For each algorithm this PR adds, replaces, or changes (search, selection,
pruning, sizing, accuracy composition, cost model, scheduling, execution
kernels): the problem with a concrete input; the old and new algorithm as
numbered steps or pseudocode naming their functions; the correctness invariant
or guarantee and the tests that check it; time and memory complexity before and
after, with a measured number when available; and assumptions, limits, and
fallbacks. Write "None" if no algorithm changes.

## Decisions and open questions

- **Needs reviewer decision**: each open question and each new public interface
  awaiting approval, with its full definition and a concrete example.
- **Decided**: each consequential decision with its date, the choice, and the
  rejected alternatives.

Write "None" when there are neither.

## Verification

| Scope | Passed / failed / ignored | Newly failing vs base | Notes |
| --- | --- | --- | --- |
| | | | |

Give each new test a one- or two-line behavioral purpose. Add the evidence the
change calls for: an execution example, a screenshot for visual output, a
before/after measurement for performance, or a diagram for architecture. Mark
non-applicable forms explicitly; do not fabricate evidence.

## Follow-ups

Linked issues, known limitations, rollout concerns, and what this PR
deliberately does not do.

## Human review — do not complete with an agent

- [ ] The MVP boundary is correct.
- [ ] New conceptual layers or public interfaces are necessary.
- [ ] The before/after description matches the intended product behavior.
- Human reviewer:
- Decision and rationale:
