---
name: asap-documentation
description: Write or revise ProjectASAP user, developer, or architecture documentation for a clearly defined audience. Use for documentation work and AI-generated prose cleanup.
---

# Documentation

Identify the intended audience before drafting:

- User documentation shows supported commands, inputs, outputs, and how to
  verify results. Omit private code-level interfaces.
- Developer documentation starts with relevant code architecture, then defines
  public inputs, outputs, functions, and data structures, followed by extension
  and verification guidance.
- Design or research documentation explains the problem, goals, constraints,
  alternatives, decisions, and observable outcomes without depending on a PR or
  issue narrative.

Match established repository terminology and writing style. Remove repetitive,
self-congratulatory, or process-oriented AI prose. Prefer concrete statements,
short sections, and examples that answer likely reader questions.

Do not expose private interfaces in public developer guidance. Verify commands,
interfaces, and links where practical before presenting them as facts.
