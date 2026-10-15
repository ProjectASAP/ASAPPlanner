## Why

Why is this needed from the user, design, correctness, or MVP perspective?

## What

What observable behavior and scope does this PR change?

## How

What is the core algorithm, interface, or design decision? Keep this top-down.

## Key code interfaces added / modified

Group by source file, with links to the files at a fixed commit. Under each group,
put one fenced code block holding the real declarations: types with every field
or variant listed (never collapsed into `// ...`), and function/method/trait
signatures with bodies elided. Mark every member (`// unchanged`, `// new`,
`// was X`, `// removed`), say on each type whether it stays or is removed, and
explain each interface in a short doc comment: what it is, what it is for, and when
it is used or produced. End with an end-to-end usage example: from what the user
writes (query, requirements, data facts, or CLI call), through where this PR's
interfaces are called, to what the user gets back, with every variable commented
and the numbers checked by running it. Write "None" if no interface changes.

## Before this PR

Describe the previous end-to-end behavior, limitation, failure mode,
architecture, or workflow.

## After this PR

Describe the new end-to-end behavior and what users, developers, or operators
can do differently.

## Evidence

Provide the appropriate evidence: execution example, screenshot for visual
output, before/after measurement for performance, or diagram for architecture.
Mark non-applicable forms explicitly; do not fabricate evidence.

## Verification

- Unit tests:
- End-to-end tests:
- Other checks:

Give each new test a one- or two-line behavioral purpose.

## Architectural decisions

List consequential decisions and rejected alternatives, or state why this is not
applicable.

## Limitations and follow-up

State known limitations, rollout concerns, and deliberately deferred work.

## Human review — do not complete with an agent

- [ ] The MVP boundary is correct.
- [ ] New conceptual layers or public interfaces are necessary.
- [ ] The before/after description matches the intended product behavior.
- Human reviewer:
- Decision and rationale:
