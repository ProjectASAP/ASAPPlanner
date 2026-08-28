---
name: asap-design-review
description: Develop or evaluate a ProjectASAP software design, especially new functionality requiring architectural decisions. Use for design documents, architecture proposals, and minimal-complexity reviews.
---

# Design review

Define the user-visible problem, inputs, outputs, constraints, and minimum viable
outcome. Separate required behavior from possible future extensibility.

For every new layer, abstraction, or public type, state the concrete pressure
that requires it and compare it with the simplest viable alternative. Look for
duplicated concepts across layers and assign one authoritative definition.

Describe end-to-end behavior and define how it will be tested before detailing
implementation. Cover maintainability, debuggability, understandability,
operational observability, and extensibility to new capabilities and features when the design materially affects them.

Define success metrics or observable proxies where practical, such as time to
diagnose, extend, or understand the system. State when a quality attribute cannot
be meaningfully measured. Define end-to-end acceptance behavior before
implementation, and distinguish independent test design from tests written after
seeing the implementation.

Record consequential decisions and rejected alternatives. Add a diagram only
when it makes relationships or execution flow materially clearer than prose.
Surface unresolved decisions that require human product or architecture input.

For a substantial design document, start from
[assets/design-document-template.md](assets/design-document-template.md).
