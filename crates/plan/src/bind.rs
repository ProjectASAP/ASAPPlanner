//! The L3→L4 binding pass (issue #98).
//!
//! Walks a canonical L3 [`QueryExpr`] and emits the sketch-bound L4 IR
//! ([`SummaryExpr`] / [`L4Node`] in `asap-sketch`). Per node, the
//! [`boundary`](crate::boundary) decision picks the realization:
//!
//! - **Sketch** — the `Aggregate` becomes a [`SummaryExpr::SummaryAgg`]
//!   carrying the committed `(SummaryKind, SummaryParams)`, wrapped in a
//!   [`SummaryExpr::SummaryEstimate`] that reads the answer back out (the
//!   `Sketch(…)` column type does not propagate past the estimate);
//! - **Exact accumulator** — a `SummaryAgg` with an exact kind
//!   (`Sum`/`Count`/`MinMax`/`Rate`/`Increase`) and no estimate: the partial
//!   state *is* the value, so L5 finalization is the identity;
//! - **Pass-through** — the whole L3 subtree is wrapped as
//!   [`SummaryExpr::Logical`], schema lifted with every column
//!   `L4DataType::Primitive`.
//!
//! Binding recurses through the `Aggregate` spine, so nested aggregates each
//! get their own decision (`quantile(0.9, sum by (svc) (rate(m[5m]))))` binds
//! KLL over an exact `Sum` accumulator over an exact `Rate` accumulator).
//!
//! ## Conservative fallbacks
//!
//! [`SummaryExpr::Logical`] boxes a whole L3 subtree — it has no L4 children —
//! so a *logical* operator above a bindable aggregate (`Filter`/`BinaryOp`/…
//! over a quantile) subsumes the aggregate into the logical wrapper unbound.
//! Rewriting through logical parents is the L4 rule engine's job (#6/#33),
//! not this pass's. Similarly conservative: multi-intent `Aggregate` nodes
//! (SQL `SELECT SUM(a), AVG(b)`) and aggregates with a `HAVING` predicate
//! (the filter would need the estimate first) stay logical.

use std::rc::Rc;

use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_ir::intent_algebra::expr_ir::ColumnRef;
use asap_ir::intent_algebra::query_expr::{BindingScope, QueryExpr, QueryExprError};
use asap_ir::intent_algebra::schema::Schema;
use asap_sketch::{
    L4DataType, L4Field, L4Node, L4Schema, SketchQuery, SummaryExpr, SummaryKind, SummaryParams,
};
use thiserror::Error;

use crate::boundary::{realize_with, Realization};
use crate::cost_model::{CostModel, DefaultCostModel};

/// Errors from the L3→L4 binding pass.
#[derive(Debug, Error)]
pub enum BindError {
    /// L3 schema derivation failed while lifting an edge to `L4Schema`.
    #[error("schema derivation failed during L3→L4 binding: {0}")]
    Schema(#[from] QueryExprError),
}

/// Bind a single query (empty `LetBinding` scope) to the L4 IR. Ranks
/// candidate summaries via [`DefaultCostModel`] (`asap-plan`'s built-in
/// static preference order, unchanged); use [`bind_with`] to plug in a
/// deployment-specific [`CostModel`] instead.
pub fn bind(expr: &QueryExpr) -> Result<Rc<L4Node>, BindError> {
    bind_in(expr, &BindingScope::default())
}

/// Bind with an explicit `LetBinding` scope — for roots that reference
/// CSE-hoisted producers via [`QueryExpr::Ref`].
pub fn bind_in(expr: &QueryExpr, scope: &BindingScope) -> Result<Rc<L4Node>, BindError> {
    bind_in_with(expr, scope, &DefaultCostModel)
}

/// Like [`bind`], but ranks candidate summaries via `cost_model` (see
/// [`crate::cost_model`]) instead of the built-in static preference order.
pub fn bind_with(expr: &QueryExpr, cost_model: &dyn CostModel) -> Result<Rc<L4Node>, BindError> {
    bind_in_with(expr, &BindingScope::default(), cost_model)
}

/// Like [`bind_in`], but ranks candidate summaries via `cost_model`.
pub fn bind_in_with(
    expr: &QueryExpr,
    scope: &BindingScope,
    cost_model: &dyn CostModel,
) -> Result<Rc<L4Node>, BindError> {
    if let QueryExpr::Aggregate {
        by,
        aggs,
        having,
        child,
        ..
    } = expr
    {
        // The bindable shape: exactly one intent, no HAVING. (Multi-intent
        // nodes and HAVING stay logical — see the module docs.)
        if let ([intent], None) = (aggs.as_slice(), having) {
            match realize_with(intent, cost_model) {
                Realization::Sketch { kind, params } => {
                    return bind_summary_agg(
                        expr, by, intent, child, kind, params, scope, true, cost_model,
                    )
                }
                Realization::ExactAccumulator { kind, params } => {
                    return bind_summary_agg(
                        expr, by, intent, child, kind, params, scope, false, cost_model,
                    )
                }
                Realization::PassThrough => {}
            }
        }
    }
    logical(expr, scope)
}

/// Emit `SummaryAgg` (recursively binding the child), plus the
/// `SummaryEstimate` readout when the realization is a sketch.
#[allow(clippy::too_many_arguments)]
fn bind_summary_agg(
    node: &QueryExpr,
    by: &[usize],
    intent: &AggIntent,
    child: &QueryExpr,
    kind: SummaryKind,
    params: SummaryParams,
    scope: &BindingScope,
    estimate: bool,
    cost_model: &dyn CostModel,
) -> Result<Rc<L4Node>, BindError> {
    let child_schema = child.output_schema_in(scope)?;
    // The single canonical L3 derivation (per-series vs cross-series, name
    // overrides) already computes the row shape; L4 only retypes the summary
    // state column.
    let out_schema = node.output_schema_in(scope)?;
    let state_idx = summary_col_index(node, &out_schema, by);

    let col = summarised_column(intent, &child_schema);
    let query = estimate.then(|| readout(intent, &col));

    let mut state_schema = lift(&out_schema);
    if let Some(field) = state_schema.fields.get_mut(state_idx) {
        field.dtype = L4DataType::Sketch(kind.clone(), params.clone());
    }

    let agg = Rc::new(L4Node {
        expr: SummaryExpr::SummaryAgg {
            child: bind_in_with(child, scope, cost_model)?,
            sketch: kind,
            params,
            col,
            by: by.to_vec(),
        },
        schema: state_schema,
    });
    match query {
        // The readout: downstream of the estimate the schema is the plain L3
        // row shape again (the `Sketch(…)` type does not propagate).
        Some(query) => Ok(Rc::new(L4Node {
            expr: SummaryExpr::SummaryEstimate {
                sketch_input: agg,
                query,
            },
            schema: lift(&out_schema),
        })),
        None => Ok(agg),
    }
}

/// Index of the summary-state column in the aggregate's output schema:
/// cross-series output is `by ++ [agg]` (the column after the keys);
/// a per-series reduction keeps every label and replaces the sample value
/// (named `value` — mirror `per_series_reduction_schema`'s fallback).
fn summary_col_index(node: &QueryExpr, out_schema: &Schema, by: &[usize]) -> usize {
    let per_series = match node {
        QueryExpr::Aggregate {
            by, aggs, child, ..
        } => {
            let is_range_child = matches!(
                child.as_ref(),
                QueryExpr::TimeRange { .. } | QueryExpr::Subquery { .. }
            );
            by.is_empty() && aggs.len() == 1 && (aggs[0].is_per_series() || is_range_child)
        }
        _ => false,
    };
    if per_series {
        out_schema
            .column_id("value")
            .or_else(|| (0..out_schema.columns.len()).find(|&i| Some(i) != out_schema.time_index))
            .unwrap_or(0)
    } else {
        by.len()
    }
}

/// The column fed into the summary: the intent's positional input column
/// resolved to a name against the child schema, or the PromQL sample value.
fn summarised_column(intent: &AggIntent, child_schema: &Schema) -> ColumnRef {
    match intent
        .input_col()
        .and_then(|id| child_schema.columns.get(id))
    {
        Some(c) => match &c.table {
            Some(t) => ColumnRef::Qualified {
                table: t.clone(),
                name: c.name.clone(),
            },
            None => ColumnRef::Named(c.name.clone()),
        },
        None => ColumnRef::SampleValue,
    }
}

/// The `SummaryEstimate` readout for a sketch-bound intent.
fn readout(intent: &AggIntent, col: &ColumnRef) -> SketchQuery {
    match intent {
        AggIntent::Quantile { q, .. } => SketchQuery::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchQuery::Cardinality,
        AggIntent::TopK { k, .. } => SketchQuery::TopK { k: *k },
        AggIntent::Count { .. } => SketchQuery::PointCount { key: col.clone() },
        other => unreachable!("no sketch realization for {other:?} (boundary::realize)"),
    }
}

/// Wrap an unrewritten L3 subtree, lifting its schema with every column
/// `L4DataType::Primitive`.
fn logical(expr: &QueryExpr, scope: &BindingScope) -> Result<Rc<L4Node>, BindError> {
    let schema = expr.output_schema_in(scope)?;
    Ok(Rc::new(L4Node {
        expr: SummaryExpr::Logical(Box::new(expr.clone())),
        schema: lift(&schema),
    }))
}

fn lift(schema: &Schema) -> L4Schema {
    L4Schema {
        fields: schema
            .columns
            .iter()
            .map(|c| L4Field {
                name: c.name.clone(),
                dtype: L4DataType::Primitive(c.dtype.clone()),
                nullable: c.nullable,
            })
            .collect(),
        time_index: schema.time_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_ir::intent_algebra::agg_intent::default_quantile;
    use asap_ir::intent_algebra::expr_ir::{CompareOp, L3Expr, L3Scalar};
    use asap_ir::intent_algebra::query_expr::{Predicate, Source};
    use asap_ir::intent_algebra::schema::{Column, DataType};
    use asap_ir::types::AccuracyTarget;
    use std::time::Duration;

    fn metric_scan(labels: &[&str]) -> QueryExpr {
        let mut columns = vec![
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
        ];
        columns.extend(labels.iter().map(|n| Column::new(*n, DataType::Utf8, true)));
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(columns, 0, vec![]),
        }
    }

    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            by: by.into(),
            aggs: vec![intent],
            output_names: vec![],
            having: None,
            child: Box::new(child),
        }
    }

    fn field<'a>(schema: &'a L4Schema, name: &str) -> &'a L4Field {
        schema
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
    }

    #[test]
    fn quantile_binds_kll_wrapped_in_estimate() {
        // quantile by (job) (m) at ε=0.01 → Estimate(Quantile) over
        // SummaryAgg(Kll{k:200}) over Logical(Scan). job = col 2.
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let root = bind(&q).unwrap();

        let SummaryExpr::SummaryEstimate {
            sketch_input,
            query,
        } = &root.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, SketchQuery::Quantile { q } if *q == 0.99));
        // Estimate edge: plain row shape — group key + Float64 answer.
        assert_eq!(
            field(&root.schema, "quantile_0_99").dtype,
            L4DataType::Primitive(DataType::Float64)
        );
        assert_eq!(
            field(&root.schema, "job").dtype,
            L4DataType::Primitive(DataType::Utf8)
        );

        let SummaryExpr::SummaryAgg {
            child,
            sketch,
            params,
            col,
            by,
        } = &sketch_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", sketch_input.expr);
        };
        assert_eq!(sketch, &SummaryKind::Kll);
        assert_eq!(params, &SummaryParams::Kll { k: 200 });
        assert_eq!(col, &ColumnRef::SampleValue);
        assert_eq!(by, &vec![2]);
        // SummaryAgg edge: the state column carries the committed (kind, params).
        assert_eq!(
            field(&sketch_input.schema, "quantile_0_99").dtype,
            L4DataType::Sketch(SummaryKind::Kll, SummaryParams::Kll { k: 200 })
        );
        assert!(matches!(child.expr, SummaryExpr::Logical(ref e)
            if matches!(**e, QueryExpr::Scan { .. })));
    }

    /// A deployment-supplied [`CostModel`] can override the default KLL
    /// choice — `bind_with` must actually consult it, not just accept and
    /// ignore it (issue: cost model interface, see `crate::cost_model`).
    struct PreferDDSketch;

    impl CostModel for PreferDDSketch {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SummaryKind],
        ) -> Vec<SummaryKind> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SummaryKind::DDSketch) {
                let ddsketch = v.remove(pos);
                v.insert(0, ddsketch);
            }
            v
        }
    }

    #[test]
    fn bind_with_custom_cost_model_overrides_default_summary_choice() {
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));

        // Default: KLL (see `quantile_binds_kll_wrapped_in_estimate` above).
        let default_root = bind(&q).unwrap();
        let SummaryExpr::SummaryEstimate { sketch_input, .. } = &default_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", default_root.expr);
        };
        let SummaryExpr::SummaryAgg { sketch, .. } = &sketch_input.expr else {
            panic!("expected SummaryAgg, got {:?}", sketch_input.expr);
        };
        assert_eq!(sketch, &SummaryKind::Kll);

        // With `PreferDDSketch`: DDSketch instead, same query.
        let custom_root = bind_with(&q, &PreferDDSketch).unwrap();
        let SummaryExpr::SummaryEstimate { sketch_input, .. } = &custom_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", custom_root.expr);
        };
        let SummaryExpr::SummaryAgg { sketch, params, .. } = &sketch_input.expr else {
            panic!("expected SummaryAgg, got {:?}", sketch_input.expr);
        };
        assert_eq!(sketch, &SummaryKind::DDSketch);
        assert_eq!(params, &SummaryParams::DDSketch { alpha: 0.01 });
    }

    #[test]
    fn exact_sum_binds_accumulator_without_estimate() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryAgg { sketch, params, .. } = &root.expr else {
            panic!(
                "expected bare SummaryAgg (no estimate), got {:?}",
                root.expr
            );
        };
        assert_eq!(sketch, &SummaryKind::Sum);
        assert_eq!(params, &SummaryParams::Sum);
        assert_eq!(
            field(&root.schema, "sum").dtype,
            L4DataType::Sketch(SummaryKind::Sum, SummaryParams::Sum)
        );
    }

    #[test]
    fn per_series_rate_keeps_labels_and_retypes_value() {
        // rate(m[5m]) — per-series: every label survives; the sample value
        // column becomes the Rate accumulator state.
        let q = agg(
            vec![],
            AggIntent::Rate,
            QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(metric_scan(&["job"])),
            },
        );
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryAgg { sketch, .. } = &root.expr else {
            panic!("expected SummaryAgg, got {:?}", root.expr);
        };
        assert_eq!(sketch, &SummaryKind::Rate);
        assert_eq!(
            root.schema
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ts", "value", "job"],
        );
        assert_eq!(
            field(&root.schema, "value").dtype,
            L4DataType::Sketch(SummaryKind::Rate, SummaryParams::Rate)
        );
        assert_eq!(root.schema.time_index, Some(0));
    }

    #[test]
    fn nested_aggregates_bind_per_node() {
        // quantile(0.9, sum by (job) (m)) — the boundary fires per node over
        // the nested tree: KLL over an exact Sum accumulator.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let root = bind(&outer).unwrap();

        let SummaryExpr::SummaryEstimate { sketch_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { child, sketch, .. } = &sketch_input.expr else {
            panic!("expected outer SummaryAgg, got {:?}", sketch_input.expr);
        };
        assert_eq!(sketch, &SummaryKind::Kll);
        let SummaryExpr::SummaryAgg {
            sketch: inner_kind,
            child: leaf,
            ..
        } = &child.expr
        else {
            panic!("expected inner SummaryAgg, got {:?}", child.expr);
        };
        assert_eq!(inner_kind, &SummaryKind::Sum);
        assert!(matches!(leaf.expr, SummaryExpr::Logical(_)));
    }

    /// Issue #115: the summary is built over the intent's own input column.
    /// Before `Cardinality`/`Quantile` carried `col`, `summarised_column` always
    /// fell through to `ColumnRef::SampleValue`, so an HLL was built over the
    /// wrong column for every SQL `COUNT(DISTINCT c)`.
    #[test]
    fn sketch_binds_the_intents_input_column() {
        // `metric_scan(&["job"])` → columns [ts=0, value=1, job=2].
        let cases = [
            (Some(2), ColumnRef::Named("job".into())),
            (Some(1), ColumnRef::Named("value".into())),
            // PromQL convention: no column ⇒ the synthetic sample value.
            (None, ColumnRef::SampleValue),
        ];
        for (col, want) in cases {
            let intent = AggIntent::Cardinality {
                col,
                accuracy: AccuracyTarget::Epsilon(0.01),
            };
            let root = bind(&agg(vec![0], intent, metric_scan(&["job"]))).unwrap();
            let bound = find_summary_col(&root)
                .unwrap_or_else(|| panic!("expected a SummaryAgg for col={col:?}"));
            assert_eq!(bound, want, "wrong summarised column for col={col:?}");
        }
    }

    /// The `col` of the first `SummaryAgg` in the tree.
    fn find_summary_col(node: &L4Node) -> Option<ColumnRef> {
        match &node.expr {
            SummaryExpr::SummaryAgg { col, .. } => Some(col.clone()),
            SummaryExpr::SummaryEstimate { sketch_input, .. } => find_summary_col(sketch_input),
            _ => None,
        }
    }

    #[test]
    fn pass_through_intents_stay_logical() {
        // avg is exact but non-mergeable; histogram_quantile (classic
        // buckets, #79) is never sketchable; exact quantile is exact by
        // decree. All three stay whole logical subtrees.
        for intent in [
            AggIntent::Avg { col: None },
            AggIntent::HistogramQuantile { q: 0.99 },
            AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Exact,
            },
        ] {
            let q = agg(vec![2], intent.clone(), metric_scan(&["job"]));
            let root = bind(&q).unwrap();
            assert!(
                matches!(root.expr, SummaryExpr::Logical(ref e) if **e == q),
                "expected Logical passthrough for {intent:?}"
            );
        }
    }

    #[test]
    fn logical_parent_subsumes_bindable_child() {
        // Filter over a bindable quantile: `Logical` has no L4 children, so
        // the conservative fallback keeps the whole subtree logical.
        let q = QueryExpr::Filter {
            pred: Predicate(L3Expr::Compare {
                left: Box::new(L3Expr::Column(0)),
                op: CompareOp::Gt,
                right: Box::new(L3Expr::Literal(L3Scalar::Float64(0.5))),
            }),
            child: Box::new(agg(vec![], default_quantile(0.99), metric_scan(&[]))),
        };
        let root = bind(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::Logical(ref e) if **e == q));
    }

    #[test]
    fn having_and_multi_intent_stay_logical() {
        let mut q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut q {
            *having = Some(Predicate(L3Expr::Literal(L3Scalar::Boolean(true))));
        }
        assert!(matches!(bind(&q).unwrap().expr, SummaryExpr::Logical(_)));

        let multi = QueryExpr::Aggregate {
            by: vec![2].into(),
            aggs: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Box::new(metric_scan(&["job"])),
        };
        assert!(matches!(
            bind(&multi).unwrap().expr,
            SummaryExpr::Logical(_)
        ));
    }

    #[test]
    fn topk_binds_cms_with_heap_and_topk_readout() {
        let q = agg(
            vec![2],
            AggIntent::TopK {
                k: 5,
                accuracy: AccuracyTarget::Epsilon(0.01),
            },
            metric_scan(&["job"]),
        );
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryEstimate {
            sketch_input,
            query,
        } = &root.expr
        else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, SketchQuery::TopK { k: 5 }));
        assert!(matches!(
            &sketch_input.expr,
            SummaryExpr::SummaryAgg {
                sketch: SummaryKind::CmsWithHeap,
                ..
            }
        ));
    }

    #[test]
    fn sql_reducer_resolves_named_input_column() {
        // SUM(bytes) over a tabular scan: `col` resolves positionally to the
        // named column, not the PromQL sample value.
        let scan = QueryExpr::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: Schema {
                columns: vec![
                    Column::new("host", DataType::Utf8, false),
                    Column::new("bytes", DataType::Int64, false),
                ],
                time_index: None,
                unique_keys: vec![],
                closed: true,
            },
        };
        let q = agg(vec![0], AggIntent::Sum { col: Some(1) }, scan);
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryAgg { col, .. } = &root.expr else {
            panic!("expected SummaryAgg, got {:?}", root.expr);
        };
        assert_eq!(col, &ColumnRef::Named("bytes".into()));
    }
}
