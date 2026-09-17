# ASAPPlanner user guide

ASAPPlanner is a reusable planning library. It translates queries into Pre-ASAP
IR and produces legal, ranked, deployment-independent Post-ASAP candidates.
Downstream systems bind those candidates to physical alternatives, make the
final deployment decision, and execute it. This follows the
[design overview](../design_docs/README.md).

This guide is a short map of the current workflows, not a new unified API.
Use the [public library functions guide](library-functions.md) for callable
functions, inputs, defaults and output limitations.

## Choose the output you need

| I want to… | Entry point | Output / valid stopping point |
| --- | --- | --- |
| Understand a query's meaning | SQL, PromQL or MetricsQL frontend | Canonical Pre-ASAP `QueryExpr`; no summary candidate search |
| Explore optimizations | `search_workload` or an explicit-strategy variant | `PlanSpace` containing discovered candidate groups |
| Compare alternatives | `PlanSpace::cost_sorted` | `RankedGroup`s with candidates and index-aligned costs; not one physical plan |
| Account for repeated demand | Recurrence-aware ranking | Ranked choices using supplied demand and horizon; no implied incremental runtime support |
| Evaluate summary-maintenance lifecycles | Lifecycle APIs with workload, capabilities and cost evidence | Lifecycle alternatives, commitments/rejections and available costs |
| Assemble a compatible semantic choice | `global_selection*`, then materialization | Selected Post-ASAP DAG; optional convenience for downstream integration |
| Inspect or export an artifact | Devtools or library export functions | A graph or versioned semantic document; serialization adds no deployment guarantee |

These stopping points serve different purposes. You do not need lifecycle analysis
just to parse a query or inspect candidates. If you intend to deploy state, its
maintenance lifecycle must be resolved and supported before physical commitment.

The primary handoff is **PlanSpace plus ranked candidates**. Do not take the first
candidate in every group and assume those choices form a feasible whole-workload
physical plan. A downstream provider can return complete physical evidence to
Planner and use `global_selection*` as a whole-plan comparison convenience.
Planner itself does not install or execute the result.

## What can I control?

| Control | Usually supplied by | Effect |
| --- | --- | --- |
| Query and source schema | Application user / frontend integration | Defines the computation and input types |
| Accuracy requirement | Application user / explicit profile | Restricts legal approximation; cost cannot override it |
| Query recurrence, predictability and time scope | Workload owner | Determines possible reuse and lifecycle demand |
| Optimization strategies | Library integrator / strategy developer | Determines which replacement opportunities are explored |
| Cost and accuracy models | Library integrator | Determines estimates, parameter sizing, guarantee checks and ordering at the stages receiving those models |
| Data statistics and empirical evidence | Evidence provider | Enables comparisons or guarantees needing those facts |
| Runtime capabilities | Downstream runtime integrator | Restricts executable lifecycle/state operations |
| Planning horizon | Application policy / integrator | Makes relevant one-time and rate costs comparable |

Choose strategies with `search_workload_with` or `search_workload_with_targets`.
Construct them with the intended models: passing a new model only to final ranking
does not redo earlier parameter sizing. The explicit strategy list is not a full
pass toggle: current workload search performs canonical sharing/CSE and derives
workload-dependent rollup automatically. See the library guide for this limitation.

## Required inputs, defaults, and compromises

Required inputs depend on the output you request. Frontend lowering needs query
text and accuracy; SQL also needs a schema catalog. Candidate search can operate
on canonical roots without a complete deployment workload. Lifecycle and physical
cost comparisons require the corresponding demand, capabilities and evidence.

| Omission or default | Meaning and compromise |
| --- | --- |
| `QueryRequirements::default()` | Exact accuracy, no response-latency bound; it does not grant permission to approximate |
| Default search/cost model | Built-in strategies, sizing and structural preferences; not a measured deployment cost prediction |
| Unknown data workload/evidence | No facts about arrival, distribution or rate are assumed; affected alternatives may be uncosted or unavailable |
| No planning horizon | Horizon-dependent lifecycle alternatives are unselectable; no arbitrary amortization period is invented |
| No empirical model/evidence | Only conclusions supported by the remaining models/evidence are available |
| Default lifecycle capabilities | **All four lifecycle flags are enabled.** Pass actual runtime support explicitly; this is not capability detection |

These are Rust API defaults. They do not mean every corresponding JSON/YAML field
can be omitted. Nor does constructing requirements automatically apply them to a
low-level API that never receives them: use the target-aware search path when
supplying per-root end-to-end accuracy requirements.

A runtime that only supports building fresh summary state from data at rest can
restrict lifecycle support to ephemeral state. Planner excludes unsupported modes.
If one legal lifecycle remains, validating and recording it is a complete decision.
Repeated queries do not imply incremental maintenance. Prepared or shared state
requires separate runtime support. See the [lifecycle recipe](library-functions.md#lifecycle-and-capabilities).

## Which steps can I skip?

- Stop after lowering when you need Pre-ASAP IR.
- Stop after search/ranking when downstream needs alternatives.
- Omit optional strategies or empirical evidence to narrow exploration; retain
  all semantic and accuracy checks needed for your promised output.
- Skip lifecycle *search* when only one supported choice exists, but still resolve
  and validate its contract before deploying state. Current lifecycle functions
  can do this with restricted capabilities; no dummy argument is needed.
- Use `global_selection*` only when you want Planner's compatible-choice helper.
  Downstream can instead consume ranked alternatives and own physical selection.
- Export only when you need inspection, persistence or a process boundary.

Calling `materialize()` constructs a selected semantic IR graph; it does not
compute summary data. A later lifecycle pass on that fixed graph does not prove
that it was the best lifecycle-aware choice among the original candidates.

## Inspect a query from the command line

Run these commands from the repository root. They are inspection tools; their
demonstration defaults are not the configuration of your downstream deployment.

### Create a query file

Create a text file containing one query per line. Prefix each query with `sql>` or `promql>`.

For example, `queries.txt`:

```text
promql> quantile(0.99, rate(http_requests_total[5m]))
sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
```

Blank lines and lines beginning with `#` are ignored.

### Show the Pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir < queries.txt
```

To dump and compare every PromQL corpus, run:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --corpora --out-dir artifacts/promql_pre_asap
```

This writes one successful-lowering JSONL dump and one error JSONL dump per
corpus, plus per-query JSON files under `<corpus>/` and `<corpus>.errors/`,
`summary.json`, and the heuristic `anomalies.md` report. The anomaly report
compares exact duplicates, whitespace/case-normalized queries, coarse
structural shapes, and distinct expressions that produce identical IR.

The corresponding SQL corpus analysis is:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --sql-corpora --out-dir artifacts/sql_pre_asap
```

### Inspect a representative Post-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
```

Or through stdin:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt
```

`show_post_asap_ir` uses an approximation target of ε = 0.01 and displays a
representative binding using the sketch strategy. It does not show the complete
ranked workload candidate set or choose a deployment lifecycle. Its SQL examples
use a fixed demonstration catalog, not your database schema. Use the library
workflow below to retain alternatives and provide your own models.

## More inspection commands

### Export a query DAG

Export pre-ASAP IR for SQL or PromQL queries for use with the interactive DAG viewer:

```sh
cargo run -p asap-devtools --bin dag_export -- --sql "<SQL query>"
```

or:

```sh
cargo run -p asap-devtools --bin dag_export -- --promql "<PromQL query>"
```

See [`tools/dag-viewer/RUNNING.md`](../../tools/dag-viewer/RUNNING.md) for instructions on running the DAG viewer.

### Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage
```

### Check sketch-replacement coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, and report what fraction of each corpus's successfully-lowered queries got a genuine sketch alternative (`SketchApproximation`, e.g. KLL vs. DDSketch) and/or a cross-query common-subexpression-reuse candidate (`CommonSubexpressionReuse`):

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --epsilon 0.01
```

`--epsilon` is optional (defaults to `0.01`). See the binary's own doc comment for exactly how "coverage" is defined and attributed back to each query.

## Additional examples

Print pre-ASAP IR for several top-k queries:

```sh
cargo run -p asap-devtools --example topk_ir
```

Print representative queries covering the pre-ASAP IR variants:

```sh
cargo run -p asap-devtools --example canonical_examples
```


## Next steps

- [Public library functions and recipes](library-functions.md)
- [Design overview and Planner/downstream boundary](../design_docs/README.md)
- [Pre-ASAP IR](../design_docs/pre-asap-ir.md)
- [Post-ASAP IR](../design_docs/post-asap-ir.md)
- [Workload demand and summary lifecycle](../design_docs/asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
