# Window and Materialization Choices

**Status:** Design proposal

**Audience:** Planner designers and implementers.

This document separates window semantics, implementation, and materialization when generating candidates for ASAPPlanner. Stage responsibilities follow [planner-layering.md](planner-layering.md); joint selection is described in [Memo-Based Joint Optimization with MILP](joint-planning-search.md).

Keep these dimensions separate:

- **Semantics:** requested window width, offset, evaluation timestamps, and alignment.
- **Implementation:** raw recomputation, supported direct sliding state, fixed-width panes, or an EH algorithm.
- **Materialization:** when each internal state is computed and how long it is retained.

A pane is an aligned, non-overlapping fixed-width interval and its aggregate/summary state. An EH bucket represents a portion of the stream under an exponential histogram algorithm, with algorithm-specific merge, expiration, and error rules.

| Implementation | Ingestion, when maintained there | Evaluation |
| --- | --- | --- |
| Direct sliding-window state | Add data and remove expired contributions. | Read state and compute result. |
| Pane-based | Update panes and evict expired panes. | Merge required panes and compute result. |
| EH-based | Update, merge, and evict buckets. | Estimate the window result. |

This table describes possible strategies. Current pane support does not imply general direct sliding-state or EH support. Offer only executable candidates. Multiple pane widths can coexist; alignment, coverage, mergeability, retention, and error contracts determine legality. A 15m window evaluated every 1m can use 1m panes; complete 5m panes alone generally cannot cover every endpoint.

Use existing `Materialization` options: `NotMaterialized`, `MatAtQuery(Memory)`, `MatAtIngestion(Memory)`, and `MatAtQueryKeptAcrossEvaluations(Memory)`. Assign them to eligible internal nodes, not necessarily the entire query. For example, maintain panes at ingestion and merge at query time. Generate only supported assignments: retained query-time state currently requires deployment capability and an eligible pane chain. Ingestion timing and upstream dependencies must satisfy Stage 2 rules.

