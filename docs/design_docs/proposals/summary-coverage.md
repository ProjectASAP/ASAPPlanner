# Summary Coverage

> Status: implemented in `ir::summary_coverage` (#567), with `SummaryMerge`
> derivation (#560) and logical transport/CSE (#537). Checking declared
> population against filters is open ([#570](https://github.com/ProjectASAP/ASAPPlanner/issues/570)).
> Audience: planner designers and architects.
> Companions: [Operator sharing](operator-sharing.md) §2.1 (schema model),
> [ASAPPlanner layering](planner-layering.md) Pass 2 (window composition).

## Goal and problem

Record **which observations a summary state was built from**, its time range and
population, so the planner can tell when combining or reusing summary states
is correct.

[Operator sharing](operator-sharing.md) §2.1 gives every edge one `Schema`. For
a summary edge, the schema records the field layout and the committed state
type, such as `(job: Utf8, state: KLL{k=200})`. It deliberately leaves out
filters, group keys and windows. That is enough while a summary is consumed
right after its producer. It stops being enough once
[planner layering](planner-layering.md) composes existing states:

- Pass 2's window-composition rule merges tumbling or EH summaries into a query
  window.
- `SummaryMerge` combines partial states.
- Sub-DAG sharing, and the reuse of ingested panes, hand one state to several
  consumers.

In these cases only the schema is left to compare. Every example below uses
two states with **equal schemas**, `Schema(job: Plain(Utf8), state: Sketch(KLL{k=200}))`.

**Example 1: time.**

| Input A | Input B | Merging A and B is… |
|---|---|---|
| latency, `[00:00, 00:01)` | latency, `[00:01, 00:02)` | correct: p99 over `[00:00, 00:02)` |
| latency, `[00:00, 00:02)` | latency, `[00:01, 00:03)` | **wrong**: `[00:01, 00:02)` is counted twice |
| latency, `[00:00, 00:01)` | latency, `[00:02, 00:03)` | correct only for `[0,1) ∪ [2,3)`, not for the continuous `[0,3)` |

`Schema.time_index` is a column position. A KLL state has no timestamp column,
so the schema cannot tell these apart.

**Example 2: population (label values).**

| Input A | Input B | Merging A and B is… |
|---|---|---|
| `region='us'` | `region='eu'` | correct: `us ∪ eu` within each `job` |
| `region='us'` | `tier='premium'` | **wrong**: premium US requests are in both |
| `region='us'` | `region='us'` | **wrong**: everything is counted twice |

`region` is a filter label, not an output column. The `job` field says how the
state is grouped, not which rows contributed.

**Example 3: time and population together.** Merging `us × [0,1)` with
`eu × [1,2)` covers exactly those two blocks. Recording one time range and one
label set would give `{us,eu} × [0,2)`, which claims data that was never read.

**Example 4: reuse.** For `p99(latency) WHERE region='us' AND ts IN [10:00, 10:05)
GROUP BY job`, a stored state with a matching schema could hold the right data,
EU data, or only 10:00–10:03. The schema shows that the state *type* fits, not
that the *contents* fit.

Schema equality is necessary but not sufficient. Without this metadata, the
planner must either refuse every composition or accept silent double counting
and missing data.

### Requirements

1. Represent time and population **jointly**, per block, never as independent
   bounds.
2. Accept a merge only when the inputs are **provably disjoint**, and fail
   closed otherwise. Merging does not imply that a summary family can remove
   duplicates.
3. Leave `Schema` and its equality unchanged.
4. Duplicate nothing the operator already records.
5. Support sources without a time column (plain tables).

## Design

### Coverage is a node property, beside the schema

```text
OperatorNode
├── schema: Schema                    what each output row looks like   (operator sharing §2.1)
├── guarantee, timing                 accuracy and execution phase      (§2.2, §2.3)
└── coverage: Option<SummaryCoverage> which observations the state holds
```

Coverage is not part of `Schema`. `SummaryMerge` requires equal input schemas,
and the inputs of every useful merge (`[0,1)` + `[1,2)`) have different coverage.
It is also not an operator parameter: a merge *derives* it from its inputs, like
the schema.

### What coverage records

A `SummaryCoverage` names one observation `source`, using the same `Source` as
`Scan`: a table or a time series. It holds a **union of regions**. Each region
pairs:

- `time_ms`: half-open bounds on the source's time column, or `None` for no
  time restriction;
- `population`: a conjunction of non-null `label = value` predicates, where
  empty means all observations.

Every observation in a region contributes once to the state. `regions = []`
means known empty coverage.

Coverage records only what no other node field records (requirement 4). What
each observation contributes and how states are grouped are already
`SummaryAgg.input` and `SummaryAgg.reduction`. `SummaryMerge` compares those on
its producers directly.

### Composition is a provably disjoint union

`merge_disjoint` accepts inputs with the same source whose regions are pairwise
disjoint. Two regions are disjoint when their time ranges do not intersect, or
when they assign different values to the same label. Different labels prove
nothing, and a region with no time bounds overlaps any region it is not
population-disjoint from. The union keeps gaps and the time/population pairing.
Adjacent intervals coalesce only when their populations are identical.

| Case | Result |
|---|---|
| `[0,1)` + `[1,2)`, same population | one region `[0,2)` |
| `[0,1)` + `[2,3)` | two regions (gap kept) |
| `[0,2)` + `[1,3)` | rejected: possible overlap |
| `region=us` + `region=eu`, same time | two regions |
| `region=us` + `region=us`, or + `tier=premium` | rejected: possible overlap |
| `us×[0,1)` + `eu×[1,2)` | two regions, never `{us,eu}×[0,2)` |
| different source | rejected |

Equality conjunctions are a deliberately narrow proof vocabulary. A richer
predicate needs an explicit disjointness rule before it can be declared.

### Lifecycle

- **Required on summary nodes.** `SummaryAgg` and `SummaryMerge` cannot pass
  structural validation without coverage. Other nodes leave it `None`. The
  field is an `Option` only because all operators share `OperatorNode`.
- **Declared at build.** The composition rule or catalog that builds a
  `SummaryAgg` declares its coverage.
- **Derived at merge.** `SummaryMerge` computes the disjoint union of its
  inputs, and validation rejects a retained value that differs.
- **Cleared on rewrite.** Rewriting a node's inputs clears its coverage, like
  other assessed metadata. The rewriter must declare it again.
- **Preserved downstream.** Logical export keeps coverage, and CSE shares two
  nodes only if their coverage is equal.

### Trust boundary

Declarations are trusted. Population is not yet checked against
`SummaryAgg.filter`, `Filter` nodes or `Scan.predicates`, so a wrong declaration
passes:

```text
A = SummaryAgg(filter: region='us'), declared {region: eu} × [0,1)   ← wrong
B = SummaryAgg(filter: region='us'), declared {region: us} × [0,1)
merge_disjoint(A, B) is accepted, and every US observation is counted twice.
```

[#570](https://github.com/ProjectASAP/ASAPPlanner/issues/570) adds the check: the
declared population must equal the `column = literal` predicates between the
`SummaryAgg` and its `Scan`. Time bounds stay trusted, because `TimeRange` is
relative to the evaluation time.

### Alternatives considered

| Alternative | Why not |
|---|---|
| Put coverage in `Schema` | Schema equality gates merges; merge inputs always differ in coverage. |
| One time range plus one label set | Invents the missing blocks (Example 3). |
| Copy `input` and `reduction` into coverage | Duplicates `SummaryAgg` and needs a consistency check; producers already carry them. |
| Arbitrary predicates per region | No general disjointness proof; overlap would be silently accepted. |
| Free-form string `source` | Two spellings of one table compare unequal; `Scan` already has `Source`. |
| Snapshot `revision` field | Deployment concern; the planner does not own catalog versions. |

## Key code interfaces

```rust
// crates/types/src/ir/summary_coverage.rs
pub struct SummaryCoverage {
    pub source: Source,               // same type as Scan.source
    pub regions: Vec<CoverageRegion>, // union; never a product of independent bounds
}
pub struct CoverageRegion {
    pub time_ms: Option<Range<i64>>,          // half-open; None = no time restriction
    pub population: BTreeMap<String, String>, // label = value AND …; empty = all
}
impl SummaryCoverage {
    pub fn validate(&self) -> Result<(), CoverageError>;
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, CoverageError>;
}
pub enum CoverageError {
    InvalidInterval, InvalidPopulation, SourceMismatch, PossibleOverlap, EmptyMerge,
    NotState, Missing,                    // node checks
    UnknownInput, MergeOutputMismatch,    // SummaryMerge (#560)
}

// crates/types/src/ir/node.rs
pub struct OperatorNode {
    // operator, result_kind, schema, guarantee, timing, …
    pub coverage: Option<SummaryCoverage>,
}
impl OperatorNode {
    pub fn with_coverage(self, c: SummaryCoverage) -> Result<Self, SchemaDerivationError>;
    pub fn requires_coverage(&self) -> bool;  // SummaryAgg, SummaryMerge
    pub fn summary_update(&self) -> Option<(&SummaryUpdate, &Reduction)>; // #560
}
// SchemaDerivationError::Coverage(CoverageError) reports every failure above.
```

`OperatorNode::validate_structure` enforces the lifecycle rules. The documented
examples are built as real `Scan → SummaryAgg → SummaryMerge` plans in
`crates/types/tests/summary_coverage_examples.rs`.

## Not covered

- **Query containment:** checking that coverage contains a requested window or
  population (Example 4). That is a later Stage 1 check that uses this data.
- **Population check:** comparing declared population with filters
  ([#570](https://github.com/ProjectASAP/ASAPPlanner/issues/570)).
- **Richer predicates:** predicates beyond non-null equality conjunctions, and
  idempotent set-union families.
- **Runtime concerns:** merge kernels, accuracy, storage and execution timing.
