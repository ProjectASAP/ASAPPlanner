# User guide: getting the pre-ASAP and post-ASAP IR for a query

Two ways to do this: no-code (the `asap-devtools` binaries) or as a library embedded in your own
codebase.

## No-code: `asap-devtools` binaries

Write a file of `sql>`/`promql>`-prefixed queries (one per line; blank lines and `#` comments
ignored):

```text
promql> quantile(0.99, rate(http_requests_total[5m]))
sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
```

**Pre-ASAP IR** (ASAP-agnostic `QueryExpr`/`AggIntent`):

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- queries.txt
```

**Post-ASAP IR** (ASAP-aware IR with `SummaryExpr`/`L4Node`, one layer downstream, with
concrete `SummaryKind`/`SummaryParams` committed per aggregate):

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
```

Both accept stdin instead of a file (`... < queries.txt`). `show_post_asap_ir` lowers at
ε = 0.01 rather than `Exact` — an exact target only ever exercises the mergeable-accumulator
arm of the binding decision, never a real sketch, so it wouldn't show the interesting case.

### Other dev tools

`crates/devtools` ships more debugging binaries and examples for poking at the lowering pipeline. Binaries (`cargo run -p asap-devtools --bin <name>`):

- **`show_pre_asap_ir`** — see above
- **`show_post_asap_ir`** — see above
- **`dag_export`** — dumps pre-ASAP IRs for given `--sql`/`--promql` queries for
  [`tools/dag-viewer`](../tools/dag-viewer/index.html), an interactive DAG viewer
  (see [`tools/dag-viewer/RUNNING.md`](../tools/dag-viewer/RUNNING.md) for
  end-to-end setup, including running it over a remote tunnel).
- **`variant_coverage`** — parses and canonicalizes every query corpus in the repo to pre-ASAP IR and reports which `QueryExpr` variants get exercised.

Examples (`cargo run -p asap-devtools --example <name>`):

- **`topk_ir`** — prints pre-ASAP IR for a hardcoded set of topk-shaped SQL/PromQL queries
- **`canonical_examples`** — prints pre-ASAP IR for one canonical query per `QueryExpr` variant, and custom join/set-op/distinct/CTE probes, to eyeball their shape.

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
front end, so it's agnostic to which language produced the tree.

```rust
use asap_aware_mapping::implement_tree;

let post_asap = implement_tree(&pre_asap)?; // Rc<L4Node> — the SummaryExpr DAG
```

Two entry points, both re-exported from the crate root:

- `implementation_for(&AggIntent) -> Implementation` — the single-node decision (sketch, exact
  accumulator, or pass-through) for one aggregation.
- `implement_tree(&QueryExpr) -> Result<Rc<L4Node>, ImplementError>` — walks a whole tree, calling
  the per-node decision at every `Aggregate` and emitting the full post-ASAP DAG.

Each has a `_with(..., &dyn CostModel)` variant. A `CostModel` is the extension point for a
deployment that wants its own candidate ranking or parameter sizing instead of this crate's
built-in static preference order (`DefaultCostModel` — what the plain, non-`_with` functions use).
See the `CostModel` trait doc in `crates/asap-aware-mapping/src/cost_model.rs` for its three
overridable hooks (`rank_candidates`, `size_params`, `realize_extension`).

### Reading the result

Match on `post_asap.expr` (a `SummaryExpr`):

- `Logical(Box<QueryExpr>)` — this subtree wasn't rewritten; execute it exactly.
- `SummaryAgg { summary, params, .. }` — an exact accumulator (`summary.is_exact()`) or an
  approximate sketch, sized to the query's `AccuracyTarget`.
- `SummaryEstimate { summary_input, query }` — wraps a sketch `SummaryAgg`; `query` is what to
  read out of it (`Quantile`, `Cardinality`, `TopK`, `PointCount`).

`docs/asap_aware_mapping.md` has the conceptual background (why this layer exists, what an
"implementation" is); `docs/pre-asap-ir.md` / `docs/post-asap-ir.md` are the node-by-node IR
reference.
