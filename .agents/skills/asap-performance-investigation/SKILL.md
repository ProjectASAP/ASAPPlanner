---
name: asap-performance-investigation
description: Investigate and improve ProjectASAP performance with reproducible before-and-after measurements. Use for latency, throughput, memory, CPU, or scalability work; not for unmeasured cleanup.
---

# Performance investigation

Define the metric, workload, environment, baseline, and acceptable regression
budget before optimizing. Reproduce the bottleneck and profile the relevant path
rather than inferring it from code shape alone.

Preserve correctness tests. Make the smallest change supported by evidence, then
repeat the same measurement enough times to expose variance. Report raw context,
before and after results, uncertainty, trade-offs, and any shifted bottleneck.

Use language-specific complexity metrics, including `rust-code-analysis`, only
when the metric answers a stated question. Do not treat a score as a quality gate
without a repository-approved threshold.
