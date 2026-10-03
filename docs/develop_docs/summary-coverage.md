# Summary coverage contract

## Problem: a schema says what a summary *is*, not what it *summarizes*

Every ASAP edge has one `Schema`. For a summary edge it records the field
layout and the committed state type:

```rust
pub struct Schema {
    pub fields: Vec<Field>,            // e.g. job: Plain(Utf8), state: Sketch(KLL{k=200}, PerSubpopulationInstance)
    pub time_index: Option<ColumnId>,  // position of a timestamp column, not a time range
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,
}
```

Nothing in it says **which time range** or **which population (label values)**
the state was built from. Filters, group keys and windows are deliberately not
`Schema` or `Field` members. This becomes a gap once the planner combines
existing summary states (`SummaryMerge`, reuse of ingested panes, sub-DAG
sharing). The producer no longer shows where a state came from, so only the
schema is left to compare. Every example below uses two states with
**exactly equal schemas**:

```text
Schema(job: Plain(Utf8), state: Sketch(KLL{k=200}, PerSubpopulationInstance)), result_kind = State
```

### Example 1: time. Equal schemas, different answers

| Input A | Input B | Merging A and B is… |
|---|---|---|
| latency, `[00:00, 00:01)` | latency, `[00:01, 00:02)` | correct: p99 over `[00:00, 00:02)` |
| latency, `[00:00, 00:02)` | latency, `[00:01, 00:03)` | **wrong**: every observation in `[00:01, 00:02)` is counted twice, which skews the quantile and doubles counts or frequencies |
| latency, `[00:00, 00:01)` | latency, `[00:02, 00:03)` | correct only for `[0,1) ∪ [2,3)`; **wrong** if used for the continuous window `[00:00, 00:03)` |

`time_index` is a column position. A KLL state has no timestamp column, so
`time_index` is `None` in all three rows and the schema cannot tell them apart.

### Example 2: population (label values). Equal schemas, different answers

| Input A | Input B | Merging A and B is… |
|---|---|---|
| `region='us'` | `region='eu'` | correct: p99 for `us ∪ eu` within each `job` |
| `region='us'` | `tier='premium'` | **wrong**: premium US requests are counted in both inputs |
| `region='us'` | `region='us'` | **wrong**: everything is counted twice |

`region` is a filter label, not an output column, so it never appears in the
schema. The `job` field only says the state is grouped by job. It does not say
which jobs or which rows contributed.

### Example 3: time and population together

A = `us × [0,1)` and B = `eu × [1,2)`. The merged state covers exactly those two
blocks. Storing a time range and a label set separately would give
`{us,eu} × [0,2)`. That claims EU data for `[0,1)` and US data for `[1,2)`
that was never read. Time and population must stay **paired per region**.

### Example 4: answering a query from a stored state

Query: `p99(latency) WHERE region='us' AND ts IN [10:00, 10:05) GROUP BY job`.
A stored state with the matching schema could hold US data for 10:00–10:05, EU
data, or US data for only 10:00–10:03. All three have the same schema. The
schema confirms that the state *type* fits, not that the *contents* fit.

**Conclusion.** Schema equality is necessary but not sufficient for composing
or reusing summaries. Without time and population metadata, the planner must
either refuse every composition or accept silent double counting and missing
data.

## The contract

`Schema` stays the layout contract and does **not** describe coverage. Coverage
is a separate field on the node, next to `schema`:

```text
OperatorNode
├── schema: Schema                    what each output row looks like
└── coverage: Option<SummaryCoverage> which observations the state holds
```

Coverage cannot live inside `Schema`. `SummaryMerge` requires equal input
schemas, and the inputs of a useful merge (`[0,1)` + `[1,2)`) always have
different coverage.

```rust
pub struct SummaryCoverage {
    pub source: String,               // observation data source; any tabular data, not necessarily time series
    pub input: SummaryUpdate,         // must equal the producing SummaryAgg.input
    pub reduction: Reduction,         // must equal the producing SummaryAgg.reduction
    pub regions: Vec<CoverageRegion>, // union of time × population blocks
}
pub struct CoverageRegion {
    pub time_ms: Option<Range<i64>>,          // half-open, on the source's time column; None = no time restriction
    pub population: BTreeMap<String, String>, // label = value AND …; empty = all observations
}
```

Rules:

- Coverage is **required on summary nodes**. `validate_structure` rejects a
  `SummaryAgg` or `SummaryMerge` whose coverage is `None` with
  `CoverageError::Missing`. Other nodes leave it `None`. The field is an
  `Option` only because all operators share `OperatorNode`.
- `with_coverage` validates the declaration, requires `State` output
  (`NotState`), and checks `input`/`reduction` against a `SummaryAgg` producer
  (`ProducerMismatch`). `validate_structure` re-checks it.
- `SummaryMerge` derives its coverage from its inputs. `validate_structure`
  rejects a retained value that differs from that union.
- Rewriting a node's inputs clears its coverage. The rewriter must declare it
  again with `with_coverage`.
- Every observation in a region contributes once to the state. `regions = []`
  means known empty coverage.
- `time_ms: None` is for sources without a time column. Such a region overlaps
  every region it is not population-disjoint from.

## Merging

`SummaryCoverage::merge_disjoint` requires equal `source`/`input`/`reduction`
and provably disjoint regions. Two regions are disjoint when their time ranges
do not intersect, or when they give different values for the same population
label. Different labels prove nothing. The examples above come out as:

| Case | Result |
|---|---|
| `[0,1)` + `[1,2)`, same population | accepted, coalesced to one region `[0,2)` |
| `[0,1)` + `[2,3)` | accepted, **two** regions (gap kept) |
| `[0,2)` + `[1,3)` | `PossibleOverlap` |
| `region=us` + `region=eu`, same time | accepted, two regions |
| `region=us` + `tier=premium` | `PossibleOverlap` |
| `us×[0,1)` + `eu×[1,2)` | accepted, two regions, never widened to `{us,eu}×[0,2)` |
| no time bounds + any region of the same population | `PossibleOverlap` |
| different source / input / reduction | `IncompatibleInput` |

Adjacent intervals coalesce only when their population maps are identical.
Merging an empty input list fails with `EmptyMerge`.

## Trusted declarations

Coverage is declared by the composition rule or catalog that built the
subtree. Nothing is inferred from SQL. Only `input` and `reduction` are checked
against the producer. Population is not compared with `SummaryAgg.filter`,
`Filter` nodes or `Scan.predicates`. Time bounds cannot be checked, because
`TimeRange` stores a relative duration. So a wrong declaration passes:

```text
A = SummaryAgg(filter: region='us', input: latency, reduction: by job)
    declared coverage: {region: eu} × [0,1)        ← wrong; the state holds US data
B = SummaryAgg(filter: region='us', input: latency, reduction: by job)
    declared coverage: {region: us} × [0,1)

merge_disjoint(A, B)  → accepted ("eu" ≠ "us" proves disjoint)
actual merged state   → every US observation in [0,1) counted twice
a query for region='eu' could also be answered from A, which holds no EU data
```

Issue #570 tracks the check. The declared population must exactly equal the
`column = literal` predicates collected between the `SummaryAgg` and its
`Scan`, and any other predicate shape fails closed. It starts strict about
which operators may sit on that path (only `Filter` and `TimeRange`), because
`Project` or `Join` can rename columns or change rows.

## Not covered

- Checking that coverage *contains* a requested query window or population
  (Example 4). That is a later query-relative check, which uses this data.
- Predicates beyond non-null equality conjunctions; idempotent set-union
  families.
- Runtime merge kernels, accuracy certificates, storage policy or execution
  timing.
