---
name: asap-pr-authoring
description: Prepare concise ProjectASAP commits and pull-request descriptions. Use when asked to organize commits, draft a PR, or explain a completed change for review.
---

# Commits and pull requests

Organize commits at incremental logical boundaries. Do not include unrelated
working-tree changes or rewrite existing history without explicit authorization.

Write the pull-request description top-down, so a reviewer learns what changed
and where to look before reading details:

1. **Summary**: three lines: the problem, the change, and its effect, with one
   concrete example and its numbers (the input, and the output, cost, or
   latency before and after).
2. **Review guide**: the base branch and, for a stacked PR, its place in the
   stack; the files or functions to read first; which parts are mechanical
   (renames, moves, regenerated snapshots) and which need careful review.
3. **Behavior: before → after**: one end-to-end example, from what the user
   writes (the workload entry or query with its requirements and data facts, or
   the CLI or devtool invocation) to what they get back, before and after this
   PR side by side. Say which user, design, correctness, or MVP need it serves.
   This replaces separate Why / What / Before / After sections; do not repeat
   it elsewhere.
4. **Interfaces**: the public interfaces the PR adds, changes, or removes:
   types, functions, methods, traits, fields, enum variants, module paths, CLI
   flags, and wire, file, or document formats. Removed interfaces appear as `-`
   lines, so no separate deleted list is needed; under the block, give a
   one-line reason for any removal whose reason is not obvious. Write "None"
   when no interface changes (for example, documentation or tests only). Use
   this layout:
   - Group by source file. Head each group with links to the files at a fixed
     commit, i.e. a permalink to the PR head or the merge commit, not a branch.
   - Under each group, put one fenced ```` ```diff ```` block. It holds the
     real declarations copied from the PR's diff (not from memory): type
     definitions with their fields or variants, and function, method and trait
     signatures with bodies elided (`;` or `/* ... */`). Mark each line by its
     first character: `+` for an added line, `-` for a removed line, and a
     space for an unchanged context line (the enclosing `struct`, `enum`,
     `impl` or `trait` header and closing brace, and unchanged members). A
     changed item is its old line(s) with `-` followed directly by its new
     line(s) with `+`; a rename is shown the same way. Do not add trailing
     `// new`, `// was X` or `// removed` comments: the `+`/`-` marks say it.
     A `///` line takes the mark of the declaration it explains (a space
     above an unchanged member, `+` above an added or changed one).
   - Never collapse members into a comment such as `// ...` or
     `/* A, B, C unchanged */`; readers take a collapsed list for a removed
     one. When a type is shown, list each of its variants or fields, unchanged
     ones as context lines, and put a `///` on the type itself saying whether
     the type stays or is removed.
   - Explain every interface. Above each declaration, write a short `///` (or
     the language's doc-comment form) saying what it is, what it is for, and
     when a caller uses it. For an enum variant or an error, say when it is
     produced and whether it reflects bad input, an unsatisfiable request, or an
     internal bug. For a field, say what it holds and its default. Keep each
     explanation to one or two lines.
   - End with a **Usage** example that is end to end, not isolated calls.
     Start from what the user writes (the workload entry or query with its
     requirements and the data facts it needs, or the CLI or devtool
     invocation). Walk through where the system reaches this PR's interfaces:
     which stage or component calls them, and with what values. End at what the
     user gets back (for a planner change: the selected plan, its sizes and
     cost, and each candidate's accepted or rejected line). Every variable gets
     a comment where it is defined saying what it holds and what it is used for
     later in the flow; no bare `let x = f(...)` lines, and never a bare
     internal call such as `size_params(...)` without the flow that reaches it.
     Use real syntax and numbers, and check them by compiling and running the
     example (a test, an example binary or a devtool run) at the linked commit.
   See [references/key-code-interfaces-example.md](references/key-code-interfaces-example.md).
5. **Algorithm changes**: for each algorithm the PR adds, replaces, or
   changes in a way that alters its results or cost (search, selection, pruning,
   sizing, accuracy composition, cost model, scheduling, execution kernels),
   write a short subsection:
   - **Problem**: what the old algorithm got wrong or could not do, with one
     concrete input and its observed outcome.
   - **Before / after**: the old and new algorithm as numbered steps or short
     pseudocode, naming the functions that implement them.
   - **Correctness**: the invariant, guarantee, or proof sketch that the new
     algorithm keeps (for example, exactness against exhaustive search, an error
     bound, or a cost that never exceeds the old one), and the tests that check
     it.
   - **Complexity**: time and memory before and after, in terms of the inputs
     that drive them, plus a measured number on a real workload when there is
     one.
   - **Assumptions and limits**: when the algorithm does not apply or falls back,
     and what it deliberately leaves out.
   Write "None" when the PR changes no algorithm (for example, a rename,
   documentation, tests, or plumbing only). Keep **Summary** as the overview and
   put the algorithmic detail here.
6. **Decisions and open questions**:
   - **Needs reviewer decision**: each open question and each new public
     interface awaiting approval, with its full definition and a concrete
     example. Keep an item here until the reviewer answers it.
   - **Decided**: each consequential decision with its date, the choice, and
     the rejected alternatives; move an item here when it is answered.
   Write "None" when there are neither.
7. **Verification**: a table with one row per test scope (crate, suite, or
   command): passed / failed / ignored, newly failing compared with the base,
   and notes. Give each new test a one- or two-line behavioral purpose, and add
   the end-to-end evidence (CLI output, snapshot changes with old → new numbers,
   measurements).
8. **Follow-ups**: linked issues, known limitations, and what the PR
   deliberately leaves out.

Use the evidence form appropriate to the change: screenshots for visual output,
execution examples for behavior, measurements for performance, or diagrams for
architecture. Explicitly mark an evidence type as not applicable instead of
fabricating it.

Never fabricate screenshots,
test results, measurements, or human approval. Creating or publishing a commit or
PR still requires user authorization.
