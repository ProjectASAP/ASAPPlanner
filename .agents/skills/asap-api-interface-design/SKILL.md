---
name: asap-api-interface-design
description: Design or review ProjectASAP public APIs and interfaces. Use when adding or changing externally consumed functions, data structures, protocols, schemas, or service boundaries.
---

# API and interface design

Define consumers, inputs, outputs, invariants, errors, lifecycle, compatibility,
and versioning. Prefer one authoritative representation and minimize conversion
layers and public surface area.

Demonstrate why a new interface is necessary and compare it with extending an
existing one. Make invalid states difficult to express where the language and
compatibility constraints permit it.

Include representative usage and failure examples. Review migration cost,
observability, security boundaries, and how developers verify integrations.
