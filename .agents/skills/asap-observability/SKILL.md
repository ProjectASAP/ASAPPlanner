---
name: asap-observability
description: Design or review ProjectASAP logs, metrics, traces, and diagnostic signals. Use for instrumentation and operational-debuggability changes.
---

# Observability

Start from operator questions and failure modes. Define which signal answers
each question, its labels or fields, units, ownership, and expected cardinality.

Prefer stable semantic events over noisy implementation logs. Avoid secrets,
personal data, unbounded labels, duplicated signals, and instrumentation that
materially changes hot-path behavior without measurement.

Show how to interpret and verify each new signal, including healthy and failure
examples. Cover dashboards or alerts only when an actionable response and owner
exist.
