---
name: asap-refactor
description: Refactor ProjectASAP code while preserving observable behavior and reducing demonstrated complexity. Use for structural cleanup without intended feature changes.
---

# Refactor

State the concrete maintenance, duplication, comprehension, or extension problem
and the behavior that must remain unchanged. Establish relevant tests before
structural changes.

Prefer removal, consolidation, and one authoritative conceptual definition over
new layers. Keep the diff focused and separate behavior changes when practical.
Use complexity metrics only as supporting evidence, not as a substitute for
review.

Verify unit and end-to-end behavior after the change. Report before and after
structure, what became simpler, and any compatibility or migration effect.
