//! The pre-ASAP → post-ASAP binding primitives (issue #98).
//!
//! This module does **not** expose a "bind me one tree" entry point.
//! [`replacement::SketchFamilyStrategy`](crate::replacement::SketchFamilyStrategy)
//! is the only public way to get bound output for a target — it always
//! returns every candidate [`ReplacementSubDAG`](crate::replacement::ReplacementSubDAG),
//! ranked; a caller that wants a single executable answer takes the first
//! entry itself (`.into_iter().next()`) and decides what to do if the list
//! is empty (this module's [`logical`] is the same pass-through fallback
//! this crate's own dispatch would otherwise use). What this module
//! provides is the shared low-level primitive that turns one *already-decided*
//! candidate into a real [`SummaryNode`]:
//!
//! - **Summary family** (sketch / sample / wavelet / statistical model) —
//!   the `Aggregate` becomes a [`SummaryExpr::SummaryAgg`] carrying the
//!   committed `family: SummaryFamilyType` (that family's own
//!   `(kind, params)`), wrapped in a [`SummaryExpr::SummaryEstimate`] that
//!   reads the answer back out (the summary-state column type does not
//!   propagate past the estimate);
//! - **Exact accumulator** — a `SummaryAgg` with `family:
//!   SummaryFamilyType::ExactAggregate` (`Sum`/`Count`/`MinMax`/`Rate`/
//!   `Increase`) and no estimate: the partial state *is* the value, so a
//!   deployment's later finalization step is the identity;
//! - **Pass-through** — the whole pre-ASAP subtree is wrapped as
//!   [`SummaryExpr::Logical`], schema lifted with every column
//!   `SummaryFamilyType::Plain`.
//!
//! Construction recurses through the `Aggregate` spine, so nested aggregates
//! each get their own independent candidate enumeration and selection
//! (`quantile(0.9, sum by (svc) (rate(m[5m]))))` binds KLL over an exact
//! `Sum` accumulator over an exact `Rate` accumulator) — see
//! [`bind_with_implementation`].
//!
//! ## Why [`implement_workload`]/[`implement_workload_with`] are still here
//!
//! Workload-level CSE sharing ([`asap_types::pre_asap::cse::share_common_subtrees`])
//! memoizes on `Rc` pointer identity: two workload roots that collapsed onto
//! the same `Rc<QueryExpr>` must resolve to the *same* canonical decision to
//! be shareable at all — there is no meaningful "N candidates" answer to
//! memoize against. So this one entry point keeps the old
//! rank-and-take-first behavior internally (via a private selector), scoped
//! to workload-wide CSE memoization specifically. It is not a general
//! "bind me one tree" API — for a single target, go through
//! `SketchFamilyStrategy` and decide what to keep yourself.
//!
//! ## Conservative fallbacks
//!
//! [`SummaryExpr::Logical`] boxes a whole pre-ASAP subtree — it has no
//! post-ASAP children — so a *logical* operator above a bindable aggregate
//! (`Filter`/`BinaryOp`/… over a quantile) subsumes the aggregate into the
//! logical wrapper unbound. Rewriting through logical parents is the
//! post-ASAP rule engine's job (#6/#33), not this pass's. Similarly
//! conservative: multi-intent `Aggregate` nodes (SQL `SELECT SUM(a), AVG(b)`)
//! and aggregates with a `HAVING` predicate (the filter would need the
//! estimate first) stay logical.

use std::rc::Rc;

use asap_types::post_asap::{
    SketchQuery, SummaryExpr, SummaryFamilyType, SummaryField, SummaryNode, SummarySchema,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, QueryExprError, Reduction};
use asap_types::pre_asap::schema::Schema;
use thiserror::Error;

use crate::cost_model::{CostModel, CseCandidate, DefaultCostModel, ShareDecision};
use crate::implementation::Implementation;
use crate::replacement::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SketchFamilyStrategy, TargetSubDAG,
};

/// Errors from the pre-ASAP → post-ASAP binding pass.
#[derive(Debug, Error)]
pub enum ImplementError {
    /// Schema derivation failed while lifting an edge to `SummarySchema`.
    #[error("schema derivation failed during pre-ASAP → post-ASAP binding: {0}")]
    Schema(#[from] QueryExprError),
}

/// The bindable shape [`crate::replacement::SketchFamilyStrategy`] targets:
/// a single intent, no `HAVING`. A multi-intent node
/// (SQL `SELECT SUM(a), AVG(b)`), or one with a `HAVING` predicate (the
/// filter would need the estimate first), stays logical — see the module
/// docs' "Conservative fallbacks".
pub fn bindable_intent(node: &QueryExpr) -> Option<&AggIntent> {
    if let QueryExpr::Aggregate {
        measures, having, ..
    } = node
    {
        if let ([intent], None) = (measures.as_slice(), having) {
            return Some(intent);
        }
    }
    None
}

/// Rank-and-take-first selector, scoped to [`implement_workload_with`]'s
/// CSE memoization and to this module's own recursion into a node's child
/// (see the module docs' "Why `implement_workload`/`implement_workload_with`
/// are still here") — **not** a general single-answer API. `root` must
/// already be the caller's own `Rc`, never fabricated per call, so this
/// never allocates beyond what the caller already held.
fn select_and_bind(
    root: &Rc<QueryExpr>,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    let target = TargetSubDAG::new(root);
    match SketchFamilyStrategy::new(cost_model)
        .replacements(&target)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Ok(node),
        Some(ReplacementSubDAG {
            replacement: Replacement::Rewrite(_),
            ..
        }) => {
            unreachable!("SketchFamilyStrategy never returns a Rewrite candidate")
        }
        // No candidate at all: `root` isn't `bindable_intent` shape (or its
        // intent has no realization `implementations_for_with` can't
        // produce — never happens, that match is exhaustive) — the same
        // conservative fallback `SketchFamilyStrategy::matches` uses.
        None => logical(root),
    }
}

/// Bind `expr` to an already-decided [`Implementation`] for its top intent.
/// The shared low-level primitive: [`select_and_bind`] (workload-CSE
/// memoization and this module's own recursion) calls this with whichever
/// candidate it kept; [`crate::replacement::SketchFamilyStrategy`] calls
/// this once per candidate it enumerates. One function turns a chosen
/// `Implementation` into a `SummaryNode`, used identically by both.
///
/// `expr` must still be the [`bindable_intent`] shape for `implementation` to
/// have any effect; anything else falls back to [`logical`]. Only `expr`'s
/// own top-level decision is forced — recursion into `expr`'s child goes
/// back through [`select_and_bind`] (fresh candidate enumeration, not a
/// forced pick), so choosing one candidate for a target never leaks into
/// that target's own nested aggregates.
pub fn bind_with_implementation(
    expr: &QueryExpr,
    implementation: Implementation,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    if let QueryExpr::Aggregate {
        reduction,
        measures,
        having,
        child,
        ..
    } = expr
    {
        // The bindable shape: exactly one intent, no HAVING. (Multi-intent
        // nodes and HAVING stay logical — see the module docs.)
        if let ([intent], None) = (measures.as_slice(), having) {
            if let Some((family, estimate)) = summary_family(implementation) {
                return bind_summary_agg(
                    expr, reduction, intent, child, family, estimate, cost_model,
                );
            }
        }
    }
    logical(expr)
}

/// Bind a whole workload's worth of already-CSE'd roots
/// ([`asap_types::pre_asap::cse::share_common_subtrees`]'s output), reusing
/// one bound [`SummaryNode`] wherever two roots share the same `Rc` *and*
/// `cost_model` decides it's worth it (issue #212, #222, #223 stages 2 and
/// 4, #237).
///
/// This is a real caller for `share_common_subtrees`, wired up deliberately:
/// the pass's own landing plan calls out that its predecessor
/// (`asap-plan::cse::dedupe_subtrees`) was deleted in #192 for being unwired
/// dead code, and lands this memoization alongside it so that never becomes
/// true again.
///
/// The memo key is `Rc::as_ptr` — pointer identity, not a second
/// `PartialEq`/structural-equality pass. `share_common_subtrees` already made
/// the (non-negotiable, `PartialEq`-checked) sharing decision; this only
/// needs to recognize when it already bound the exact `Rc` a later root
/// hands back, and is deliberately *not* a general "does an available
/// `Implementation` satisfy this one" lookup — that subsumption question is
/// `asap_aware_mapping::implementation::Matcher`'s documented, deliberately-unfilled
/// job, not this one's.
///
/// A first pass over `roots` counts each distinct `Rc<QueryExpr>` pointer's
/// true `consumer_count` across the whole workload, so the
/// [`CseCandidate`]/[`CostModel::cse_share_decision`] cost comparison (see
/// `docs/design_docs/cse-cost-model-decision.md`) sees the real total, not a running
/// count that grows as roots are processed left to right. The decision is
/// made once, the first time a shared pointer is bound, and cached alongside
/// the bound `SummaryNode` so every later occurrence of that same `Rc`
/// applies the same decision consistently — either every consumer reuses one
/// shared `SummaryNode`, or every consumer (including the first) binds
/// independently.
///
/// Only whole-root sharing is memoized (matching two workload roots that are
/// themselves the same `Rc<QueryExpr>` after CSE) — a root is bound at most
/// once per distinct root pointer when the decision is
/// `Share`, but it still walks each such tree's own internal structure
/// fresh; a subtree shared only *below* two different roots' top level does
/// not additionally memoize inside that walk. Widening this to sub-root
/// memoization is future work.
pub fn implement_workload<Id>(
    roots: Vec<(Id, Rc<QueryExpr>)>,
) -> Vec<(Id, Result<Rc<SummaryNode>, ImplementError>)> {
    implement_workload_with(roots, &DefaultCostModel)
}

/// Like [`implement_workload`], but ranks candidate summaries — and decides
/// CSE sharing — via `cost_model` instead of the built-in defaults (see
/// [`crate::cost_model`]).
pub fn implement_workload_with<Id>(
    roots: Vec<(Id, Rc<QueryExpr>)>,
    cost_model: &dyn CostModel,
) -> Vec<(Id, Result<Rc<SummaryNode>, ImplementError>)> {
    let mut consumer_count: std::collections::HashMap<*const QueryExpr, usize> =
        std::collections::HashMap::new();
    for (_, expr) in &roots {
        *consumer_count.entry(Rc::as_ptr(expr)).or_insert(0) += 1;
    }

    let mut memo: std::collections::HashMap<*const QueryExpr, (Rc<SummaryNode>, ShareDecision)> =
        std::collections::HashMap::new();
    roots
        .into_iter()
        .map(|(id, expr)| {
            let ptr = Rc::as_ptr(&expr);
            let result = match memo.get(&ptr) {
                Some((cached, ShareDecision::Share)) => Ok(Rc::clone(cached)),
                Some((_, ShareDecision::RecomputeIndependently)) => {
                    select_and_bind(&expr, cost_model)
                }
                None => select_and_bind(&expr, cost_model).inspect(|node| {
                    let count = consumer_count[&ptr];
                    let decision = if count > 1 {
                        let candidate = CseCandidate {
                            subtree: &expr,
                            bound_summary: node,
                            consumer_count: count,
                        };
                        cost_model.cse_share_decision(&candidate)
                    } else {
                        // Only one consumer: nothing to compare against, and
                        // this branch is never consulted again for `ptr`.
                        ShareDecision::Share
                    };
                    memo.insert(ptr, (Rc::clone(node), decision));
                }),
            };
            (id, result)
        })
        .collect()
}

/// Translate an [`Implementation`] into the `(family, needs a
/// SummaryEstimate readout)` pair [`bind_summary_agg`] needs, or `None` for
/// `PassThrough` (the caller falls back to [`logical`]).
///
/// Every family's partial state needs a readout to recover a value, except
/// `ExactAggregate` — its partial state *is* the value already, so no
/// estimate step follows it.
fn summary_family(implementation: Implementation) -> Option<(SummaryFamilyType, bool)> {
    Some(match implementation {
        Implementation::ExactAggregate { kind, params } => {
            (SummaryFamilyType::ExactAggregate(kind, params), false)
        }
        Implementation::Sketch { kind, params } => (SummaryFamilyType::Sketch(kind, params), true),
        Implementation::Sample { kind, params } => (SummaryFamilyType::Sample(kind, params), true),
        Implementation::Wavelet { kind, params } => {
            (SummaryFamilyType::Wavelet(kind, params), true)
        }
        Implementation::StatModel { kind, params } => {
            (SummaryFamilyType::StatModel(kind, params), true)
        }
        Implementation::PassThrough => return None,
    })
}

/// Emit `SummaryAgg` (recursively binding the child), plus the
/// `SummaryEstimate` readout when `estimate` is set.
#[allow(clippy::too_many_arguments)]
fn bind_summary_agg(
    node: &QueryExpr,
    reduction: &Reduction,
    intent: &AggIntent,
    child: &Rc<QueryExpr>,
    family: SummaryFamilyType,
    estimate: bool,
    cost_model: &dyn CostModel,
) -> Result<Rc<SummaryNode>, ImplementError> {
    let child_schema = child.output_schema()?;
    // The single canonical pre-ASAP derivation (per-series vs cross-series,
    // name overrides) already computes the row shape; binding only retypes
    // the summary state column.
    let per_series = matches!(reduction, Reduction::PerEntity);
    let by: Vec<usize> = reduction
        .group_keys()
        .map(|g| g.to_vec())
        .unwrap_or_default();
    let out_schema = node.output_schema()?;
    let state_idx = summary_col_index(&out_schema, &by, per_series);

    let col = summarised_column(intent, &child_schema);
    let query = estimate.then(|| readout(intent, &col, cost_model));

    let mut state_schema = lift(&out_schema);
    if let Some(field) = state_schema.fields.get_mut(state_idx) {
        field.dtype = family.clone();
    }

    // `reduction` is carried onto `SummaryAgg` verbatim — not flattened to a
    // bare `Vec<ColumnId>` — so `SummaryExecutor::find_candidates` can tell
    // a genuine empty-`by` reduction apart from a per-entity shape with no
    // grouping concept at all (issue #163). `bind_summary_agg` is the single
    // place that decides this; nothing downstream re-derives it.
    let agg = Rc::new(SummaryNode {
        expr: SummaryExpr::SummaryAgg {
            child: select_and_bind(child, cost_model)?,
            family,
            col,
            reduction: reduction.clone(),
        },
        schema: state_schema,
    });
    match query {
        // The readout: downstream of the estimate the schema is the plain
        // pre-ASAP row shape again (the summary-state type does not
        // propagate).
        Some(query) => Ok(Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: agg,
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
/// `per_series` is the caller's already-read `Reduction` (issue #165) —
/// this never re-derives it, so it can't disagree with the caller.
fn summary_col_index(out_schema: &Schema, by: &[usize], per_series: bool) -> usize {
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

/// The `SummaryEstimate` readout for a summary-bound intent.
fn readout(intent: &AggIntent, col: &ColumnRef, cost_model: &dyn CostModel) -> SketchQuery {
    match intent {
        AggIntent::Quantile { q, .. } => SketchQuery::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchQuery::Cardinality,
        AggIntent::TopK { k, .. } => SketchQuery::TopK { k: *k },
        AggIntent::Count { .. } => SketchQuery::PointCount {
            key: col.clone(),
            value: None,
        },
        // Core doesn't know the shape of a deployment-specific `Extension`
        // intent, so it can't build its readout either — delegate to the
        // same `CostModel` that decided (via `realize_extension`) this
        // intent gets a summary realization at all. See `readout_extension`'s
        // doc for the invariant this depends on.
        AggIntent::Extension { ext_kind, payload } => {
            cost_model.readout_extension(ext_kind, payload, col)
        }
        other => {
            unreachable!(
                "no summary realization for {other:?} (implementation::implementations_for_with)"
            )
        }
    }
}

/// Wrap an unrewritten pre-ASAP subtree, lifting its schema with every column
/// `SummaryFamilyType::Plain`. Public so a caller can fall back to this
/// explicitly — e.g. when `SketchFamilyStrategy::replacements()` returns no
/// candidate for a target, or a deployment wants to force a node its own
/// runtime can't actually implement — through the same fallback this
/// crate's own dispatch uses, without duplicating the schema-lift logic.
pub fn logical(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
    let schema = expr.output_schema()?;
    Ok(Rc::new(SummaryNode {
        expr: SummaryExpr::Logical(Box::new(expr.clone())),
        schema: lift(&schema),
    }))
}

fn lift(schema: &Schema) -> SummarySchema {
    SummarySchema {
        fields: schema
            .columns
            .iter()
            .map(|c| SummaryField {
                name: c.name.clone(),
                dtype: SummaryFamilyType::Plain(c.dtype.clone()),
                nullable: c.nullable,
            })
            .collect(),
        time_index: schema.time_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::{ExactKind, ExactParams, SketchKind, SketchParams};
    use asap_types::pre_asap::agg_intent::default_quantile;
    use asap_types::pre_asap::expr_ir::{CompareOpKind, ScalarValue};
    use asap_types::pre_asap::query_expr::{Predicate, Source};
    use asap_types::pre_asap::schema::{Column, DataType};
    use asap_types::types::AccuracyTarget;
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

    /// A cross-series reduction, grouped by `by` (possibly empty — a
    /// genuine full reduction, never "no grouping concept").
    fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    /// A per-entity reduction: no grouping concept at all (issue #165).
    fn agg_per_entity(intent: AggIntent, child: QueryExpr) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            having: None,
            child: Rc::new(child),
        }
    }

    fn field<'a>(schema: &'a SummarySchema, name: &str) -> &'a SummaryField {
        schema
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
    }

    /// This module no longer exposes a single-answer public API — the tests
    /// below use the same "rank via `cost_model`, keep the first candidate"
    /// pattern `select_and_bind` already implements, since `bind.rs`'s own
    /// tests are the one internal caller allowed to reach it directly.
    /// An external caller doesn't have this shortcut — it goes through
    /// `SketchFamilyStrategy::replacements()` itself (see the module docs).
    fn bind_first(
        expr: &QueryExpr,
        cost_model: &dyn CostModel,
    ) -> Result<Rc<SummaryNode>, ImplementError> {
        select_and_bind(&Rc::new(expr.clone()), cost_model)
    }

    fn bind(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
        bind_first(expr, &DefaultCostModel)
    }

    #[test]
    fn quantile_binds_kll_wrapped_in_estimate() {
        // quantile by (job) (m) at ε=0.01 → Estimate(Quantile) over
        // SummaryAgg(Kll{k:200}) over Logical(Scan). job = col 2.
        let q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        let root = bind(&q).unwrap();

        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, SketchQuery::Quantile { q } if *q == 0.99));
        // Estimate edge: plain row shape — group key + Float64 answer.
        assert_eq!(
            field(&root.schema, "quantile_0_99").dtype,
            SummaryFamilyType::Plain(DataType::Float64)
        );
        assert_eq!(
            field(&root.schema, "job").dtype,
            SummaryFamilyType::Plain(DataType::Utf8)
        );

        let SummaryExpr::SummaryAgg {
            child,
            family,
            col,
            reduction,
        } = &summary_input.expr
        else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(SketchKind::Kll, SketchParams::Kll { k: 200 })
        );
        assert_eq!(col, &ColumnRef::SampleValue);
        assert_eq!(reduction, &Reduction::by(vec![2]));
        // SummaryAgg edge: the state column carries the committed family.
        assert_eq!(
            field(&summary_input.schema, "quantile_0_99").dtype,
            SummaryFamilyType::Sketch(SketchKind::Kll, SketchParams::Kll { k: 200 })
        );
        assert!(matches!(child.expr, SummaryExpr::Logical(ref e)
            if matches!(**e, QueryExpr::Scan { .. })));
    }

    /// A deployment-supplied [`CostModel`] can override the default KLL
    /// choice — `bind_first` (via `select_and_bind`) must actually consult it,
    /// not just accept and ignore it (issue: cost model interface, see
    /// `crate::cost_model`).
    struct PreferDDSketch;

    impl CostModel for PreferDDSketch {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            let mut v = candidates.to_vec();
            if let Some(pos) = v.iter().position(|k| *k == SketchKind::DDSketch) {
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
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &default_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", default_root.expr);
        };
        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert!(matches!(
            family,
            SummaryFamilyType::Sketch(SketchKind::Kll, _)
        ));

        // With `PreferDDSketch`: DDSketch instead, same query.
        let custom_root = bind_first(&q, &PreferDDSketch).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &custom_root.expr else {
            panic!("expected SummaryEstimate root, got {:?}", custom_root.expr);
        };
        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(
                SketchKind::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 }
            )
        );
    }

    /// A deployment-supplied `CostModel` can realize an `AggIntent::Extension`
    /// intent as a real sketch instead of the default `PassThrough` (issue
    /// #150) — `implementations_for_with` must consult `realize_extension`
    /// for the `Extension` arm, and `readout` must consult
    /// `readout_extension` to build its `SketchQuery` without panicking.
    struct FrequencyCostModel;

    impl CostModel for FrequencyCostModel {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchKind],
        ) -> Vec<SketchKind> {
            candidates.to_vec()
        }

        fn realize_extension(
            &self,
            ext_kind: &str,
            _payload: &serde_json::Value,
        ) -> crate::implementation::Implementation {
            if ext_kind == "frequency" {
                crate::implementation::Implementation::Sketch {
                    kind: SketchKind::CountSketch,
                    params: SketchParams::CountSketch {
                        width: 256,
                        depth: 4,
                    },
                }
            } else {
                crate::implementation::Implementation::PassThrough
            }
        }

        fn readout_extension(
            &self,
            ext_kind: &str,
            payload: &serde_json::Value,
            _col: &ColumnRef,
        ) -> SketchQuery {
            assert_eq!(ext_kind, "frequency");
            let value = payload["item"].as_str().map(str::to_string);
            SketchQuery::PointCount {
                key: ColumnRef::Named("item".into()),
                value,
            }
        }
    }

    #[test]
    fn extension_intent_stays_logical_by_default() {
        // Without a CostModel overriding `realize_extension`, an
        // `Extension` intent must stay `PassThrough` -- today's behavior,
        // unchanged.
        let intent = AggIntent::Extension {
            ext_kind: "frequency".to_string(),
            payload: serde_json::json!({ "item": "checkout" }),
        };
        let q = agg(vec![], intent, metric_scan(&[]));
        let root = bind(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::Logical(_)));
    }

    #[test]
    fn extension_intent_binds_via_custom_cost_model() {
        let intent = AggIntent::Extension {
            ext_kind: "frequency".to_string(),
            payload: serde_json::json!({ "item": "checkout" }),
        };
        let q = agg(vec![], intent, metric_scan(&[]));
        let root = bind_first(&q, &FrequencyCostModel).unwrap();

        let SummaryExpr::SummaryEstimate {
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected SummaryEstimate root, got {:?}", root.expr);
        };
        assert!(matches!(
            query,
            SketchQuery::PointCount { key: ColumnRef::Named(k), value: Some(v) }
                if k == "item" && v == "checkout"
        ));

        let SummaryExpr::SummaryAgg { family, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::Sketch(
                SketchKind::CountSketch,
                SketchParams::CountSketch {
                    width: 256,
                    depth: 4
                }
            )
        );
    }

    #[test]
    fn exact_sum_binds_accumulator_without_estimate() {
        let q = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryAgg { family, .. } = &root.expr else {
            panic!(
                "expected bare SummaryAgg (no estimate), got {:?}",
                root.expr
            );
        };
        assert_eq!(
            family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert_eq!(
            field(&root.schema, "sum").dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
    }

    #[test]
    fn per_series_rate_keeps_labels_and_retypes_value() {
        // rate(m[5m]) — per-series: every label survives; the sample value
        // column becomes the Rate accumulator state.
        let q = agg_per_entity(
            AggIntent::Rate,
            QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Rc::new(metric_scan(&["job"])),
            },
        );
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryAgg { family, .. } = &root.expr else {
            panic!("expected SummaryAgg, got {:?}", root.expr);
        };
        assert_eq!(
            family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
        );
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
            SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
        );
        assert_eq!(root.schema.time_index, Some(0));
    }

    /// Issue #163, case 1: a bare per-series range function (e.g.
    /// `quantile_over_time(...)`) binds to `SummaryAgg { reduction:
    /// PerEntity, .. }` — proving the pre-ASAP `Reduction` this crate
    /// already computes (issue #165) is carried onto the post-ASAP node
    /// verbatim, not flattened back into an ambiguous bare `Vec<ColumnId>`.
    #[test]
    fn bare_per_series_aggregate_binds_summary_agg_with_per_entity_reduction() {
        let q = agg_per_entity(
            default_quantile(0.99),
            QueryExpr::TimeRange {
                range: Duration::from_secs(10),
                child: Rc::new(metric_scan(&["job"])),
            },
        );
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { reduction, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(reduction, &Reduction::PerEntity);
    }

    /// Issue #163, case 2: an aggregation operator explicitly invoked with
    /// no `by(...)` (e.g. `count(hll_metric)`) binds to `SummaryAgg {
    /// reduction: Reduce(vec![]), .. }` — byte-identical `by: []` to the
    /// previous test at the old `Vec<ColumnId>` shape; `reduction` is what
    /// tells them apart now.
    #[test]
    fn explicit_empty_by_aggregate_binds_summary_agg_with_reduce_reduction() {
        let intent = AggIntent::Cardinality {
            col: None,
            accuracy: AccuracyTarget::Epsilon(0.01),
        };
        let q = agg(vec![], intent, metric_scan(&["job"]));
        let root = bind(&q).unwrap();
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { reduction, .. } = &summary_input.expr else {
            panic!("expected SummaryAgg, got {:?}", summary_input.expr);
        };
        assert_eq!(reduction, &Reduction::by(vec![]));
    }

    #[test]
    fn nested_aggregates_bind_per_node() {
        // quantile(0.9, sum by (job) (m)) — the implementation decision
        // fires per node over the nested tree: KLL over an exact Sum
        // accumulator.
        let inner = agg(vec![2], AggIntent::Sum { col: None }, metric_scan(&["job"]));
        let outer = agg(vec![], default_quantile(0.9), inner);
        let root = bind(&outer).unwrap();

        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        let SummaryExpr::SummaryAgg { child, family, .. } = &summary_input.expr else {
            panic!("expected outer SummaryAgg, got {:?}", summary_input.expr);
        };
        assert!(matches!(
            family,
            SummaryFamilyType::Sketch(SketchKind::Kll, _)
        ));
        let SummaryExpr::SummaryAgg {
            family: inner_family,
            child: leaf,
            ..
        } = &child.expr
        else {
            panic!("expected inner SummaryAgg, got {:?}", child.expr);
        };
        assert_eq!(
            inner_family,
            &SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
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
    fn find_summary_col(node: &SummaryNode) -> Option<ColumnRef> {
        match &node.expr {
            SummaryExpr::SummaryAgg { col, .. } => Some(col.clone()),
            SummaryExpr::SummaryEstimate { summary_input, .. } => find_summary_col(summary_input),
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
        // Filter over a bindable quantile: `Logical` has no post-ASAP
        // children, so the conservative fallback keeps the whole subtree
        // logical.
        let q = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)),
                op: CompareOpKind::Gt,
                right: Rc::new(QueryExpr::Literal(ScalarValue::Float64(0.5))),
            })),
            child: Rc::new(agg(vec![], default_quantile(0.99), metric_scan(&[]))),
        };
        let root = bind(&q).unwrap();
        assert!(matches!(root.expr, SummaryExpr::Logical(ref e) if **e == q));
    }

    #[test]
    fn having_and_multi_intent_stay_logical() {
        let mut q = agg(vec![2], default_quantile(0.99), metric_scan(&["job"]));
        if let QueryExpr::Aggregate { having, .. } = &mut q {
            *having = Some(Predicate(Rc::new(QueryExpr::Literal(
                ScalarValue::Boolean(true),
            ))));
        }
        assert!(matches!(bind(&q).unwrap().expr, SummaryExpr::Logical(_)));

        let multi = QueryExpr::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }, AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(metric_scan(&["job"])),
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
            summary_input,
            query,
        } = &root.expr
        else {
            panic!("expected estimate root, got {:?}", root.expr);
        };
        assert!(matches!(query, SketchQuery::TopK { k: 5 }));
        assert!(matches!(
            &summary_input.expr,
            SummaryExpr::SummaryAgg {
                family: SummaryFamilyType::Sketch(SketchKind::CmsWithHeap, _),
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

    /// Issue #237, #223 stage 4: a `CostModel` that declines CSE sharing
    /// makes `implement_workload_with` bind each occurrence independently,
    /// even though the two roots are the exact same `Rc<QueryExpr>` (as
    /// `share_common_subtrees` would hand back for two identical workload
    /// entries) — the opposite of `DefaultCostModel`'s unconditional-share
    /// behavior pinned by `crates/integration-tests/tests/cse.rs`.
    #[test]
    fn implement_workload_with_recomputes_independently_when_cost_model_declines_sharing() {
        struct NeverShareCse;
        impl CostModel for NeverShareCse {
            fn rank_candidates(
                &self,
                _intent: &AggIntent,
                candidates: &[SketchKind],
            ) -> Vec<SketchKind> {
                candidates.to_vec()
            }
            fn cse_share_decision(&self, _candidate: &CseCandidate) -> ShareDecision {
                ShareDecision::RecomputeIndependently
            }
        }

        let shared = Rc::new(agg(vec![2], default_quantile(0.99), metric_scan(&["job"])));
        let bound = implement_workload_with(
            vec![("a", Rc::clone(&shared)), ("b", Rc::clone(&shared))],
            &NeverShareCse,
        );
        let [(_, ra), (_, rb)] = bound.as_slice() else {
            panic!("expected 2 bound results");
        };
        let ra = ra.as_ref().expect("a failed to bind");
        let rb = rb.as_ref().expect("b failed to bind");
        assert!(
            !Rc::ptr_eq(ra, rb),
            "a CostModel that declines CSE sharing must bind each occurrence \
             independently, even for two roots that are the same Rc<QueryExpr>"
        );
    }
}
