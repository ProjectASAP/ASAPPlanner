---
name: asap-pr-authoring
description: Prepare concise ProjectASAP commits and pull-request descriptions. Use when asked to organize commits, draft a PR, or explain a completed change for review.
---

# Commits and pull requests

Organize commits at incremental logical boundaries. Do not include unrelated
working-tree changes or rewrite existing history without explicit authorization.

Write the pull-request description top-down:

1. **Why**: the user, design, correctness, or MVP need.
2. **What**: the observable behavior and scope changed.
3. **How**: the core implementation or algorithm, without narrating every file.
4. **Key code interfaces added / modified**: the public interfaces the PR adds,
   changes, or removes: types, functions, methods, traits, fields, enum
   variants, module paths, CLI flags, and wire, file, or document formats.
   Write "None" when no interface changes (for example, documentation or tests
   only). Use this layout:
   - Group by source file. Head each group with links to the files at a fixed
     commit, i.e. a permalink to the PR head or the merge commit, not a branch.
   - Under each group, put one fenced code block in the source language. It
     holds the real declarations copied from the diff: type definitions with
     their fields or variants, and function, method and trait signatures with
     bodies elided (`;` or `/* ... */`). Mark each change with a trailing
     comment (`// new`, `// was X`, `// removed`).
   - Never collapse members into a comment such as `// ...` or
     `/* A, B, C unchanged */`; readers take a collapsed list for a removed
     one. When a type is shown, list each of its variants or fields with a
     one-line `///` and a marker (`// unchanged`, `// new`, `// was X`,
     `// removed`), and put a `///` on the type itself saying whether the type
     stays or is removed.
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
5. **Major algorithm changes**: for each algorithm the PR adds, replaces, or
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
   documentation, tests, or plumbing only). Keep **How** as the overview and put
   the algorithmic detail here.
6. **Before this PR**: the previous end-to-end behavior, limitation, failure
   mode, architecture, or workflow.
7. **After this PR**: the new end-to-end behavior and what users, developers, or
   operators can do differently.
8. **Verification**: focused tests and applicable end-to-end evidence.

Include a compact before/after example, screenshot, or diagram when it provides
real evidence or makes behavior easier to understand. For significant design
changes, summarize architectural decisions and alternatives.

Use the evidence form appropriate to the change: screenshots for visual output,
execution examples for behavior, measurements for performance, or diagrams for
architecture. Explicitly mark an evidence type as not applicable instead of
fabricating it.

State limitations and follow-up work explicitly. Never fabricate screenshots,
test results, measurements, or human approval. Creating or publishing a commit or
PR still requires user authorization.
