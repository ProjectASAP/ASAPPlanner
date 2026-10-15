---
name: asap-issue-authoring
description: Write ProjectASAP GitHub issues that a reader understands without prior context. Use when asked to open, draft or rewrite an issue, a tracking issue, or a follow-up.
---

# Issues

Write the issue top-down so a reader new to the topic understands the problem
before the solution:

1. **Problem, by example.** Start with one concrete, minimal example of the
   problem: the input (a query, a workload, a command, a data shape), what
   happens today, and what should happen instead. Use real syntax and real
   numbers; show the current and the desired plan, output, or behavior side by
   side when that is clearer. Prefer one small example over an abstract
   description, and add a second only when it shows a different case.
2. **Why it happens / why it isn't done today.** The cause in the code or the
   design, named precisely (types, functions, rules), and what already works
   nearby.
3. **Proposal or goal.** What a fix or feature would change, at the level of
   interfaces and behavior, with alternatives when the choice is open.
4. **Scope and acceptance.** What is in and out of scope, and the test that
   would show it is done (for a bug, the regression test that fails today).
5. **Status and related work.** Decisions already made (with dates), why it is
   deferred if it is, and links to related issues and PRs.

Never invent behavior or numbers: derive the example from the code, a run, or
a measurement, and say which. Keep people and company names out. Use the
repository's issue templates when one fits, and keep their sections.
