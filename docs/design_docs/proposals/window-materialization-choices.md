# Window and Materialization Choices

**Status:** Design proposal

**Audience:** Planner designers and implementers.

This document separates window semantics, implementation, and materialization when generating candidates for ASAPPlanner. Stage responsibilities follow [planner-layering.md](planner-layering.md); joint selection is described in [Memo-Based Joint Optimization with MILP](joint-planning-search.md).

Keep these dimensions separate:

- **Semantics:** requested window width, offset, evaluation timestamps, and alignment.
- **Implementation:** raw recomputation, supported direct sliding state, fixed-width tumbling windows, or an EH algorithm.
- **Materialization:** when each internal state is computed and how long it is retained.

A tumbling window is an aligned, non-overlapping fixed-width interval and its aggregate/summary state. An EH bucket represents a portion of the stream under an exponential histogram algorithm, with algorithm-specific merge, expiration, and error rules.

| Implementation | Ingestion, when maintained there | Evaluation |
| --- | --- | --- |
| Direct sliding-window state | Add data and remove expired contributions. | Read state and compute result. |
| Tumbling-window-based | Update tumbling windows and evict expired tumbling windows. | Merge required tumbling windows and compute result. |
| EH-based | Update, merge, and evict buckets. | Estimate the window result. |

This table describes possible strategies. Current tumbling window support does not imply general direct sliding-state or EH support. Offer only executable candidates. Multiple tumbling-window widths can coexist; alignment, coverage, mergeability, retention, and error contracts determine legality. A 15m window evaluated every 1m can use 1m tumbling windows; complete 5m tumbling windows alone generally cannot cover every endpoint.

Use existing physical properties: `Lifetime::None`, `(QueryTime, OneEvaluation(Memory))`, `(IngestionTime, AcrossEvaluations(Memory))`, and `(QueryTime, AcrossEvaluations(Memory))`. Assign them to eligible internal nodes, not necessarily the entire query. For example, maintain tumbling windows at ingestion and merge at query time. Generate only supported assignments: retained query-time state currently requires deployment capability and an eligible tumbling-window chain. Ingestion timing and upstream dependencies must satisfy Stage 2 rules.

