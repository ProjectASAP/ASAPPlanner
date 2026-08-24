# User guide: getting the pre-ASAP and post-ASAP IR for a query

The `asap-devtools` package provides command-line tools for inspecting the pre-ASAP and post-ASAP IR generated for SQL and PromQL queries.

## 1. Create a query file

Create a text file containing one query per line. Prefix each query with `sql>` or `promql>`.

For example, `queries.txt`:

```text
promql> quantile(0.99, rate(http_requests_total[5m]))
sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
```

Blank lines and lines beginning with `#` are ignored.

## 2. Show the pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir < queries.txt
```

## 3. Show the post-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
```

Or through stdin:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt
```

`show_post_asap_ir` uses an approximation target of ε = 0.01 so that the output can exercise sketch-based implementations rather than only exact aggregation.

## Other useful commands

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

## Additional examples

Print pre-ASAP IR for several top-k queries:

```sh
cargo run -p asap-devtools --example topk_ir
```

Print representative queries covering the pre-ASAP IR variants:

```sh
cargo run -p asap-devtools --example canonical_examples
```

## As a library

Neither crate is published to crates.io — depend on them by path (inside this workspace) or by git:

```toml
# from another crate in this workspace
asap-frontend-promql = { path = "../frontend-promql" }   # or asap-frontend-sql
asap-aware-mapping = { path = "../asap-aware-mapping" }

# from an external codebase
asap-frontend-promql = { git = "https://github.com/ProjectASAP/ASAPPlanner", package = "asap-frontend-promql" }
asap-aware-mapping = { git = "https://github.com/ProjectASAP/ASAPPlanner", package = "asap-aware-mapping" }
```

### Step 1 — get the pre-ASAP IR

Lower a query string with a front end. Front ends never depend on each other or on the binder —
pull only the one you need.

```rust
use asap_frontend_promql::lower_promql;
use asap_types::types::AccuracyTarget;

let pre_asap = lower_promql(
    "quantile(0.99, rate(http_requests_total[5m]))",
    AccuracyTarget::Epsilon(0.01),
)?; // QueryExpr
```

(SQL: `asap_frontend_sql::lower_sql(query, &catalog, accuracy).await` — needs a `SqlCatalog`
describing your tables; see `crates/devtools/src/bin/show_pre_asap_ir.rs` for a worked example.)

`AccuracyTarget` travels with the query, not the crate — pass `Exact` for no approximation
allowed, `Epsilon(e)` / `EpsilonDelta{epsilon, delta}` otherwise.

### Step 2 — get the post-ASAP IR

Feed the `QueryExpr` to `asap-aware-mapping`. This crate depends only on `asap-types`, never on a
front end, so it's agnostic to which language produced the tree. There is no "bind me one tree"
entry point: `SketchAlgorithmStrategy::replacements()` always returns every valid candidate for a
target, ranked, and you take the one you want.

```rust
use asap_aware_mapping::{Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy, TargetSubDAG};
use std::rc::Rc;

let root = Rc::new(pre_asap);
let target = TargetSubDAG::new(&root);
let candidates = SketchAlgorithmStrategy::default_cost_model().replacements(&target);

// Take the cost-model-preferred candidate — the common case.
let Some(ReplacementSubDAG { replacement: Replacement::Summary(post_asap), .. }) =
    candidates.into_iter().next()
else {
    // No candidate (e.g. the node isn't a bindable Aggregate) — fall back to
    // `asap_aware_mapping::replacement::keep_pre_asap(&root)`, the same conservative
    // pass-through this crate's own dispatch uses.
    panic!("no candidate for this target");
};
// post_asap: Rc<SummaryNode> — the SummaryExpr DAG
```

`implementations_for_with(&AggIntent, &dyn CostModel) -> Vec<Implementation>` is the lower-level
enumeration `SketchAlgorithmStrategy` wraps, if you only need the per-node decision (sketch, exact
accumulator, or pass-through) without binding it into a `SummaryNode`.

`SketchAlgorithmStrategy::new(&dyn CostModel)` (vs. `default_cost_model()`) is the extension point for
a deployment that wants its own candidate ranking or parameter sizing instead of this crate's
built-in static preference order (`DefaultCostModel` — what `default_cost_model()` uses).
See the `CostModel` trait doc in `crates/asap-aware-mapping/src/cost_model.rs` for its overridable
hooks (`rank_candidates`, `size_params`, `realize_extension`, …).

To see every root of a whole workload at once — including the candidates CSE-shared subtrees get
(a shared subtree's `MemoGroup` carries both the "share" and "recompute independently" options,
ranked by `CostModel::cse_share_decision`) — use `asap_aware_mapping::search_workload`/
`search_workload_with` instead; unlike the single-target path above, these return every discovered
site's full candidate list (a `PlanSpace`), not one picked winner. Committing to one final,
physically-materialized `SummaryNode` per shared subtree is out of this crate's scope — that's a
downstream deployment's call, once it also knows where each candidate would be placed.

### Reading the result

Match on `post_asap.expr` (a `SummaryExpr`):

- `KeepPreAsap(Box<QueryExpr>)` — this subtree wasn't rewritten; execute it exactly.
- `SummaryAgg { summary, params, .. }` — an exact accumulator (`summary.is_exact()`) or an
  approximate sketch, sized to the query's `AccuracyTarget`.
- `SummaryEstimate { summary_input, query }` — wraps a sketch `SummaryAgg`; `query` is what to
  read out of it (`Quantile`, `Cardinality`, `TopK`, `PointCount`).

`docs/design_docs/asap_aware_mapping.md` has the conceptual background (why this layer exists, what an
"implementation" is); `docs/design_docs/pre-asap-ir.md` / `docs/design_docs/post-asap-ir.md` are the node-by-node IR
reference.
