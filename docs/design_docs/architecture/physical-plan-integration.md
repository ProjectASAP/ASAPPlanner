# Physical plan integration

## Purpose

This document defines the boundary between ASAPPlanner's logical plans,
physical lowering, statistics resolution, and analytical resource estimation.
It answers which representation is authoritative at each stage and prevents
the cost model from being coupled directly to either logical IR.

The integration pipeline is:

```text
pre-ASAP QueryExpr  ─┐
                     ├─ physical lowering ─> PhysicalOperator DAG
post-ASAP SummaryExpr┘                              │
                                                    v
                                           OperatorStatistics
                                                    │
                                                    v
                                            ResourceEstimate
```

The arrows are boundaries, not casts. A logical node does not necessarily
become one physical node, and a physical operator is not merely a renamed
logical variant.

## Sources of truth

Each representation is authoritative for a different concern:

| Representation | Authoritative concern |
|---|---|
| `QueryExpr` | Original exact query semantics: sources, predicates, relational and PromQL operations, and output shape. |
| `SummaryExpr` | Logical summary semantics: selected family, grouping strategy, summary composition, and summary readout. |
| `PhysicalOperator` DAG | Selected executable algorithms, their configuration, physical identity, edges, and execution multiplicity. |
| `OperatorStatistics` | Workload-dependent evidence required by each selected physical operator's resource formula. |
| `ResourceEstimate` | Estimated CPU operations, peak live memory, and physical source/disk reads over one comparison scope. |

`PhysicalOperator` is therefore the source of truth for the operator vocabulary
consumed by analytical costing. `OperatorStatistics` corresponds one-to-one
with that vocabulary. It must not independently invent operator kinds or copy
all variants from either logical IR.

The canonical physical-plan types should live at a neutral boundary shared by
lowering, costing, explanation, and downstream compilation. Their conceptual
ownership is not the cost formula implementation, even if an intermediate
implementation keeps the Rust type in the cost module while the interface is
being established.

## Why logical and physical variants are not one-to-one

One logical operation may choose between algorithms or expand into a physical
sub-DAG. Conversely, one physical operator may implement nodes originating
from either logical IR.

Examples include:

- `Sort` followed by `Limit` may lower to an in-memory comparison sort plus a
  limit, or to one heap Top-K operator configured by limit and offset.
- `Join` may lower to a hash join with an explicit build side or to another
  supported join algorithm.
- `SummaryAgg` may lower to an exact accumulator build, CMS build, KLL build,
  or another physical summary algorithm selected by the candidate.
- `SummaryEstimate` must lower to a readout operator compatible with the
  concrete summary state it consumes.
- shared logical sub-DAGs become shared physical nodes only when they refer to
  the same physical identity and compatible evidence.

For this reason, aligning `OperatorStatistics` directly with `QueryExpr` would
lose post-ASAP summary implementations, while aligning it directly with
`SummaryExpr` would lose raw query operators and physical algorithm choices.

## Lowering obligations

Physical lowering is complete only when it recursively lowers the entire
selected candidate DAG. It must:

1. preserve the semantics and source coverage of the logical candidate;
2. select an explicit physical algorithm for every logical operation;
3. carry algorithm configuration on the physical operator rather than in a
   generic statistics record;
4. connect every physical edge and preserve child order for non-commutative
   operators;
5. assign stable physical identities so shared sub-DAGs can be counted once;
6. assign execution multiplicity and retained/transient state ownership;
7. reject the complete candidate if any logical operation has no supported
   physical lowering.

Keeping an unsupported logical node outside the physical DAG and costing only
its modeled descendants is invalid because it undercounts the candidate.

### Pre-ASAP lowering

`KeepPreAsap` recursively lowers its contained `QueryExpr`. Typical physical
operators include scans, filters, projections, hash aggregates, joins,
ordering, bounded Top-K, limits, and PromQL-specific operators. The selected
physical algorithm, rather than the logical spelling, determines the formula.

### Post-ASAP lowering

Every `SummaryExpr` operation also needs explicit physical realization:

| Logical summary operation | Required physical realization |
|---|---|
| `SummaryAgg` | concrete summary or exact-state build/update operator, including family parameters and grouping layout |
| `SummaryJoin` | concrete summary-join algorithm and state layout |
| `SummaryMerge` | merge operator over compatible concrete summary states |
| `SummarySubtract` | subtract operator supported by the selected state representation |
| `SummaryDelete` | physical deletion/update operator supported by the selected representation |
| `SummaryEstimate` | family- and query-specific readout operator |
| `KeepPreAsap` | recursive lowering of the contained `QueryExpr` |

This table is a completeness requirement, not a claim that every realization
already exists. Until lowering introduces an explicit physical operator,
statistics contract, validation rule, and resource formula for an operation,
a candidate containing it is unavailable.

The streaming integration can consume a complete binding through
`StreamingNodeEvidence`. That binding is keyed to exact `SummaryNode`
identities and uses structured evidence for aggregate state, join, merge,
subtract, delete, readout, and retained pre-ASAP work. It is a physical
evidence boundary, not automatic physical lowering: a deployment must still
select each concrete implementation and provide all edges, resource facts,
multiplicities, source ownership, and stable physical identities. The planner
fails closed when any reachable `SummaryExpr` node lacks that binding.

The raw/query portion of a streaming comparison remains a `PhysicalDag` using
the canonical `PhysicalOperator` and `OperatorStatistics` pairing. Summary
evidence is kept separate only where lifecycle-driven update, retention, and
expiration multiplicities require facts beyond the query-DAG
`Once`/`PerEvaluation` schedule. It must not redefine workload, lifecycle, or
summary-family semantics.

Lifecycle choice affects the physical DAG but does not replace it. Ephemeral,
prepared, shared, and continuously maintained alternatives determine when
build, update, readout, merge, subtract, or delete nodes execute. The physical
operators still determine how each execution consumes CPU, memory, and I/O.

## Operator and evidence reference

The [developer reference](../../develop_docs/physical-operator-reference.md)
defines operator-specific configuration, statistics, and validation requirements.
Every physical variant needs matching evidence and a supported resource formula.

## Comparison and failure behavior

Raw and post-ASAP alternatives must cover the same `ComparisonScope`: arrival
mode, planning time, workload horizon, recurrence, event-time selection,
logical sources, ordinary predicates or info-metric matchers, and physical
source snapshots.

Missing statistics, stale evidence, an unsupported physical algorithm,
unlowered logical operations, inconsistent edges, or different comparison
scopes make the complete candidate unavailable. The integration must never
replace those failures with zero cost or structural node counting.

See [Analytical resource cost](../proposals/asap-aware-mapping/analytical-resource-cost.md) for the resource
formulas, evidence validation, comparison-scope rules, and calibration model.

## Conditional temporal-average lowering

`avg_over_time(a[5m])` can expose independently maintained sum and count
components, but their division is conditional. Two finite samples of `1e308`
have a finite average even though their sum overflows. Planner therefore emits
a read-time `BinaryOperator` with `checked_finite_division=true` and never exports
this temporal transformation as an unconditional pre-ASAP rewrite.

The backend lowers the guard to `FiniteDiv`: operands and quotient must be finite,
and the divisor must be nonzero. Failure executes the original average query.
Zero and subnormal averages remain valid accelerated results. This guard is
distinct from `checked_relative_division`, whose relative-error certificate also
requires a normal result; setting both guards or attaching a guard to a non-division
operator is invalid. Compilers must preserve this typed condition rather than
recovering average semantics from query text.
