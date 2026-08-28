---
name: asap-implement-existing-design
description: Implement ProjectASAP functionality from an approved existing design. Use when interfaces and intended behavior are already decided; route unresolved architecture back to design review.
---

# Implement an existing design

Locate the authoritative design and restate its observable inputs, outputs,
invariants, and acceptance behavior. Do not silently redesign public interfaces.

Implement the smallest vertical slice that satisfies the approved design. Keep
concept definitions authoritative across layers and avoid speculative extension
points. Add focused unit and end-to-end tests with short behavior descriptions.

If implementation exposes a material design gap or contradiction, stop that
portion, document the decision needed, and request design review instead of
embedding an unreviewed architectural choice.
