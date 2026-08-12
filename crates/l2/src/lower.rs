//! Layer-2 → canonical L3 IR converter.
//!
//! Recursively converts a whole [`relational::QueryExpr`] tree into a whole
//! [`query_expr::QueryExpr`] tree. The single-statistic sketchable `Aggregate`
//! fuses directly in canonical terms (window-swap, positional `Aggregate.by`);
//! see the `Aggregate` arm.
//!
//! Name resolution is an explicit pass: [`convert_root`] runs the
//! [`Binder`](crate::binder) first to build the complete, self-contained
//! schema every `ColumnId` indexes into, so positional resolution downstream
//! is total.

use std::time::Duration;

use thiserror::Error;

use crate::binder::{collect_referenced_columns, Binder};
use crate::column_resolution::{
    output_schema_for_aggregate, resolve_column_refs, resolve_expr, resolve_group_keys_promql,
    ResolveError,
};
use crate::relational::{AggFunc, QueryExpr as LQueryExpr, SourceSpec};
use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_ir::intent_algebra::expr_ir::{ColumnRef, L2Expr, L3Expr, L3Scalar};
use asap_ir::intent_algebra::names::BindingName;
use asap_ir::intent_algebra::query_expr::{
    GroupKeys, Predicate, ProjectItem, QueryExpr as CQueryExpr, Reduction, SortKey, Source,
};
use asap_ir::intent_algebra::schema::{ColumnId, Schema};
use asap_ir::types::AccuracyTarget;

/// Errors produced while converting a Layer-2 tree to canonical.
#[derive(Debug, Error)]
pub enum ConvertError {
    /// A column reference (`Aggregate` key, `TopK` key, aggregate input column)
    /// did not resolve against the child's derived schema.
    #[error("column resolution failed: {0}")]
    Resolve(#[from] ResolveError),
    /// Deriving the schema of an already-converted child failed (needed to
    /// resolve positional column references against it).
    #[error("schema derivation failed: {0}")]
    Schema(#[from] asap_ir::intent_algebra::query_expr::QueryExprError),
    /// Group keys landed on a per-series windowed/range reduction, which must
    /// stay label-preserving (`Aggregate.by` empty). The only PromQL shape that
    /// would do this is a generic `topk by (…)`, whose grouping is routed to
    /// `Sort.partition_by` instead (issue #12) — so this indicates an
    /// unsupported query shape rather than a path that should silently group.
    #[error(
        "group keys on a per-series windowed reduction are unsupported \
             (per-group ranking must use Sort.partition_by — see issue #12)"
    )]
    WindowedReductionKeys,
    /// A counter-derivative range function (`changes`/`delta`/`deriv`/…) reached
    /// the converter without an enclosing `Window`, so no range could be
    /// recovered. Emitting it range-less would silently drop its window, so this
    /// signals a malformed L2 tree rather than a valid instant aggregate (#71).
    #[error(
        "counter-derivative range function has no window \
             (its range would be silently dropped)"
    )]
    RangelessRangeReduction,
}

/// Lower a Layer-2 tree to canonical L3, threading `accuracy` onto every
/// approximate intent (`Count`, `Quantile`, `Cardinality`, `TopK`).
pub fn convert_root(
    legacy: &LQueryExpr,
    accuracy: &AccuracyTarget,
) -> Result<CQueryExpr, ConvertError> {
    convert_root_with_inherited(legacy, accuracy, &[])
}

/// [`convert_root`] with label names inherited from an enclosing scope seeded
/// into the leaf schema. Used when re-binding a `BinaryOp` side, so an outer
/// aggregate's group keys (`sum by (__name__)(a or b)`) resolve even though they
/// appear in neither side's own sub-tree (issue #52).
fn convert_root_with_inherited(
    legacy: &LQueryExpr,
    accuracy: &AccuracyTarget,
    inherited: &[String],
) -> Result<CQueryExpr, ConvertError> {
    let fallback = Binder::new().bind_with_inherited(legacy, inherited);
    let l3 = convert(legacy, &fallback, accuracy)?;
    // Both language front ends end here, so this is the one place to normalize
    // structural differences between equivalent queries (issue #34).
    Ok(crate::canonicalize::canonicalize(l3))
}

/// Convert a Layer-2 tree to canonical L3.
///
/// `fallback` is the leaf schema used for schema-less (PromQL) `Source`s —
/// the Binder's usage-derived `(ts, value)` floor + referenced labels. SQL
/// leaves carry their own resolved schema on [`SourceSpec::schema`], so the
/// fallback is unused for them. Positional column references (`Aggregate`
/// keys + input columns, `TopK` keys) resolve against the **converted child's
/// derived output schema**, so a `JOIN`'s concatenated schema and a table's
/// real columns bind to the right positions.
pub fn convert(
    legacy: &LQueryExpr,
    fallback: &Schema,
    acc: &AccuracyTarget,
) -> Result<CQueryExpr, ConvertError> {
    Ok(match legacy {
        LQueryExpr::Source(spec) => scan(spec, fallback, &[])?,

        LQueryExpr::Scalar(v) => CQueryExpr::Scalar(*v),

        LQueryExpr::EvalTime => CQueryExpr::EvalTime,

        // `vector(s)` / `scalar(v)` — the type-conversion bridges. Convert the
        // child through the same fallback schema and wrap it (issue #48).
        LQueryExpr::VectorFromScalar(inner) => {
            CQueryExpr::VectorFromScalar(Box::new(convert(inner, fallback, acc)?))
        }
        LQueryExpr::ScalarFromVector(inner) => {
            CQueryExpr::ScalarFromVector(Box::new(convert(inner, fallback, acc)?))
        }

        // ρ — relabel. Resolve the label-value expression positionally against
        // the child's schema (source labels seeded by the binder), then rewrite
        // `dst` over the converted child (issue #50).
        LQueryExpr::Relabel { dst, value, input } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            CQueryExpr::Relabel {
                dst: dst.clone(),
                value: resolve_expr(value, &child_schema)?,
                child: Box::new(child),
            }
        }

        // Series sampling (`limitk`/`limit_ratio`) — resolve the grouping keys
        // positionally, pass the sampled child through unchanged (issue #86).
        LQueryExpr::Sample { keys, kind, input } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            CQueryExpr::Sample {
                by: resolve_column_refs(keys, &child_schema)?.into(),
                kind: *kind,
                child: Box::new(child),
            }
        }

        // `info(v, [selector])` — label enrichment. The selector matchers are
        // info-metric-side and symbolic (not resolved against the child), so
        // they copy straight through; L4 resolves the join keys (issue #84).
        LQueryExpr::InfoJoin { selector, input } => CQueryExpr::InfoJoin {
            selector: selector.clone(),
            child: Box::new(convert(input, fallback, acc)?),
        },

        LQueryExpr::Ref(name) => CQueryExpr::Ref {
            name: BindingName::new(name.clone()),
        },

        // Fold label matchers / pushed-down predicates directly onto the Scan
        // when the immediate child is a `Source`; otherwise emit a `Filter`.
        // Predicate column refs resolve positionally against the input schema.
        LQueryExpr::Filter { pred, input } => match input.as_ref() {
            LQueryExpr::Source(spec) => scan(spec, fallback, pred.conjuncts())?,
            other => {
                let child = convert(other, fallback, acc)?;
                let child_schema = child.output_schema()?;
                CQueryExpr::Filter {
                    pred: Predicate(resolve_expr(pred, &child_schema)?),
                    child: Box::new(child),
                }
            }
        },

        LQueryExpr::Aggregate {
            keys,
            without,
            aggs,
            having,
            input,
        } => {
            // Single-statistic aggregate (no HAVING) over a *time-series* leaf
            // fuses into the canonical shape: a `Window` input becomes
            // `Window { Aggregate { by: [] } }` (a per-series, label-preserving
            // reduction — see `output_schema_in`'s `Window` arm). GROUP BY keys
            // resolve *positionally* into `Aggregate.by` — the same shape SQL
            // produces — whenever they're in scope: an instant selector, or a
            // label-preserving per-series `rate`/`increase`/`*_over_time`. A
            // windowed reduction must stay label-preserving (no group keys); the
            // one PromQL shape that would group here, generic `topk by (…)`,
            // routes its grouping to `Sort.partition_by` instead (see below).
            // The reducer's input column resolves against the aggregate's
            // *direct* input (the scan under any window). (SQL GROUP BY is
            // tabular, so it skips this branch for the plain `Aggregate.by` path
            // below.)
            if aggs.len() == 1 && having.is_none() && !input.leaf_is_tabular() {
                // Extract the temporal range. Two sources:
                //   1. An L2 `Window` child (emitted for `*_over_time` functions).
                //   2. `Rate`/`Increase` AggFunc (carry their own range; no L2 Window).
                // The range becomes a `TimeRange` node wrapping the inner scan
                // rather than a `Window` node above the aggregate — `Window` is
                // reserved for streaming query-repetition semantics.
                let (agg_input_l2, time_range): (&LQueryExpr, Option<Duration>) =
                    match input.as_ref() {
                        LQueryExpr::Window {
                            duration,
                            input: win_input,
                            ..
                        } => (win_input, Some(*duration)),
                        // A `PromQLSubquery` is itself the range context (a range
                        // function over a sub-query, `f(<inst>[range:res])` —
                        // issues #42/#55). No separate `TimeRange`, and
                        // `Rate`/`Increase`'s carried window is subsumed by the
                        // sub-query's range.
                        other @ LQueryExpr::PromQLSubquery { .. } => (other, None),
                        other => {
                            let range = match &aggs[0].func {
                                AggFunc::Rate { window } | AggFunc::Increase { window } => {
                                    Some(*window)
                                }
                                _ => None,
                            };
                            (other, range)
                        }
                    };
                // A counter-derivative range function is per-series over a range
                // and always arrives under an L2 `Window` (or, for a sub-query
                // argument, over a `PromQLSubquery` — handled above). If one
                // reaches here range-less over anything else, emitting it without
                // a `TimeRange` would silently drop its window — reject the
                // malformed tree instead (issue #71).
                if time_range.is_none()
                    && !matches!(agg_input_l2, LQueryExpr::PromQLSubquery { .. })
                    && matches!(
                        &aggs[0].func,
                        AggFunc::Changes
                            | AggFunc::Delta
                            | AggFunc::IDelta
                            | AggFunc::Deriv
                            | AggFunc::Resets
                            | AggFunc::PredictLinear { .. }
                            | AggFunc::DoubleExpSmoothing { .. }
                            | AggFunc::LastOverTime
                            | AggFunc::FirstOverTime
                            | AggFunc::MadOverTime
                            | AggFunc::TsOfMinOverTime
                            | AggFunc::TsOfMaxOverTime
                            | AggFunc::TsOfFirstOverTime
                            | AggFunc::TsOfLastOverTime
                    )
                {
                    return Err(ConvertError::RangelessRangeReduction);
                }
                let agg_child_raw = convert(agg_input_l2, fallback, acc)?;
                let agg_in_schema = agg_child_raw.output_schema()?;
                let intent = agg_func_to_intent(
                    &aggs[0].func,
                    acc,
                    resolve_agg_col(&aggs[0].col, &agg_in_schema)?,
                );
                // Wrap the child in TimeRange when there is a range.
                let agg_child = match time_range {
                    Some(range) => CQueryExpr::TimeRange {
                        range,
                        child: Box::new(agg_child_raw),
                    },
                    None => agg_child_raw,
                };
                // Resolve the group keys positionally against the aggregate's
                // input so the grouping lives in `Aggregate.reduction` — the
                // *same* shape SQL produces. Only for instant (non-range)
                // aggregates: an instant aggregate (`sum by (job) (m)`) or a
                // cross-series reduction over a label-preserving `rate`/`increase`.
                //
                // A range reduction's keys can't go into `by`: this node must stay
                // a per-entity (label-preserving) reduction. So a windowed
                // reduction must carry no group keys: the only PromQL shape that
                // would put keys here is a generic `topk by (…)`, and that routes
                // its grouping to `Sort.partition_by` instead (issue #12). Any keys
                // still reaching a windowed reduction are an unsupported shape —
                // surface it rather than silently dropping the grouping.
                if time_range.is_some() && !keys.is_empty() {
                    return Err(ConvertError::WindowedReductionKeys);
                }
                // PromQL absent-label grouping semantics (issue #53): a key
                // provably absent from a *closed* input schema (e.g. the output
                // of a nested cross-series aggregate that collapsed the label)
                // groups every series into one partition and is omitted from
                // the output — drop it instead of rejecting the query.
                let by = group_keys(resolve_group_keys_promql(keys, &agg_in_schema)?, *without);
                // Decide the reduction kind once, right here, rather than
                // leaving a downstream consumer to re-derive it (issue #165):
                // a per-entity reduction is a single intent with no grouping
                // keys at all, whose intent is inherently per-series
                // (`rate`/`increase`/…) or whose child is a range-window
                // wrapper (`TimeRange`/`Subquery` — the `*_over_time` family).
                // `without ()` is still a genuine reduction (groups by every
                // label), never per-entity, even with an empty exclusion list.
                let is_range_child = matches!(
                    agg_child,
                    CQueryExpr::TimeRange { .. } | CQueryExpr::Subquery { .. }
                );
                let per_entity =
                    by.is_empty() && !by.is_without() && (intent.is_per_series() || is_range_child);
                let reduction = if per_entity {
                    Reduction::PerEntity
                } else {
                    Reduction::Reduce(by)
                };
                return Ok(CQueryExpr::Aggregate {
                    reduction,
                    aggs: vec![intent],
                    output_names: vec![aggs[0].alias.clone().unwrap_or_default()],
                    having: None,
                    child: Box::new(agg_child),
                });
            }

            // Plain canonical `Aggregate`: multi-agg or HAVING-bearing. Keys +
            // per-reducer input columns resolve against the child's (input)
            // schema; HAVING references the aggregate's *output* columns, so it
            // resolves against the derived output schema instead.
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let by = group_keys(resolve_column_refs(keys, &child_schema)?, *without);
            let intents: Vec<AggIntent> = aggs
                .iter()
                .map(|item| -> Result<AggIntent, ConvertError> {
                    let col = resolve_agg_col(&item.col, &child_schema)?;
                    Ok(agg_func_to_intent(&item.func, acc, col))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let output_names: Vec<String> = aggs
                .iter()
                .map(|item| item.alias.clone().unwrap_or_default())
                .collect();
            let having = having
                .as_ref()
                .map(|h| -> Result<Predicate, ConvertError> {
                    let out_schema =
                        output_schema_for_aggregate(&child_schema, &by, &intents, &output_names)?;
                    Ok(Predicate(resolve_expr(h, &out_schema)?))
                })
                .transpose()?;
            CQueryExpr::Aggregate {
                // Always a genuine reduction: this branch is multi-intent or
                // HAVING-bearing, and per-entity reductions are always a
                // single, HAVING-less intent (handled above).
                reduction: Reduction::Reduce(by),
                aggs: intents,
                output_names,
                having,
                child: Box::new(child),
            }
        }

        // Standalone L2 Window (bare range vector `m[5m]` not inside a
        // recognized single-stat Aggregate): map to TimeRange.
        LQueryExpr::Window {
            duration, input, ..
        } => CQueryExpr::TimeRange {
            range: *duration,
            child: Box::new(convert(input, fallback, acc)?),
        },

        // π — resolve each project item's expression to positional against the
        // child's schema.
        LQueryExpr::Project {
            cols,
            qualifier,
            input,
        } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let cols = cols
                .iter()
                .map(|item| -> Result<ProjectItem, ConvertError> {
                    Ok(ProjectItem {
                        alias: item.alias.clone(),
                        expr: resolve_expr(&item.expr, &child_schema)?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            CQueryExpr::Project {
                cols,
                qualifier: qualifier.clone(),
                child: Box::new(child),
            }
        }

        LQueryExpr::Distinct { cols, input } => {
            // Resolve the L2 (name-based) dedup keys to positional ids against
            // the converted child's schema, like every other L3 column ref.
            let child = convert(input, fallback, acc)?;
            let cols = resolve_column_refs(cols, &child.output_schema()?)?;
            CQueryExpr::Distinct {
                cols,
                child: Box::new(child),
            }
        }

        LQueryExpr::TopK { k, by, input } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let by: GroupKeys = resolve_column_refs(by, &child_schema)?.into();
            CQueryExpr::Aggregate {
                // A ranking always reduces (a `by`-empty TopK ranks the whole
                // input into one ordering, never per-entity).
                reduction: Reduction::Reduce(by),
                aggs: vec![AggIntent::TopK {
                    k: *k as usize,
                    accuracy: acc.clone(),
                }],
                output_names: vec![],
                having: None,
                child: Box::new(child),
            }
        }

        LQueryExpr::Merge { inputs } => CQueryExpr::Merge {
            children: inputs
                .iter()
                .map(|i| convert(i, fallback, acc))
                .collect::<Result<Vec<_>, _>>()?,
        },

        LQueryExpr::Join {
            kind,
            pred,
            left,
            right,
        } => {
            // Each branch is bound independently (different leaves / label sets).
            let left = convert_root(left, acc)?;
            let right = convert_root(right, acc)?;
            // The join predicate resolves against the concatenated left++right
            // schema (the Join's own output shape), so left refs land at
            // 0..left_len and right refs at left_len.. .
            let mut concat = left.output_schema()?;
            concat.columns.extend(right.output_schema()?.columns);
            let pred = match pred {
                Some(p) => Predicate(resolve_expr(p, &concat)?),
                None => Predicate(L3Expr::Literal(L3Scalar::Boolean(true))),
            };
            CQueryExpr::Join {
                kind: kind.clone(),
                pred,
                left: Box::new(left),
                right: Box::new(right),
            }
        }

        LQueryExpr::SetOp {
            kind,
            all,
            left,
            right,
        } => CQueryExpr::SetOp {
            kind: kind.clone(),
            all: *all,
            left: Box::new(convert_root(left, acc)?),
            right: Box::new(convert_root(right, acc)?),
        },

        LQueryExpr::Sort {
            keys,
            partition_by,
            input,
        } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let keys = keys
                .iter()
                .map(|k| -> Result<SortKey, ConvertError> {
                    Ok(SortKey {
                        expr: resolve_expr(&k.expr, &child_schema)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Per-group ordering keys (`topk by (…)`) resolve positionally
            // against the child schema — the per-series reduction below
            // preserves its label columns, so the grouping label is present.
            let partition_by: GroupKeys = resolve_column_refs(partition_by, &child_schema)?.into();
            CQueryExpr::Sort {
                keys,
                partition_by,
                child: Box::new(child),
            }
        }

        LQueryExpr::Limit { n, offset, input } => CQueryExpr::Limit {
            n: *n as usize,
            offset: *offset as usize,
            child: Box::new(convert(input, fallback, acc)?),
        },

        LQueryExpr::LetBinding { name, expr, body } => CQueryExpr::LetBinding {
            name: BindingName::new(name.clone()),
            expr: Box::new(convert(expr, fallback, acc)?),
            child: Box::new(convert(body, fallback, acc)?),
        },

        LQueryExpr::PromQLSubquery {
            range,
            resolution,
            input,
        } => CQueryExpr::Subquery {
            range: *range,
            resolution: *resolution,
            child: Box::new(convert(input, fallback, acc)?),
        },

        // Analytic window: args / partition-by / order-by resolve positionally
        // against the child's output schema.
        LQueryExpr::WindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            input,
        } => {
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let args = args
                .iter()
                .map(|a| resolve_expr(a, &child_schema))
                .collect::<Result<Vec<_>, _>>()?;
            let partition_by: GroupKeys = resolve_column_refs(partition_by, &child_schema)?.into();
            let order_by = order_by
                .iter()
                .map(|k| -> Result<SortKey, ConvertError> {
                    Ok(SortKey {
                        expr: resolve_expr(&k.expr, &child_schema)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            CQueryExpr::WindowFunc {
                func: func.clone(),
                args,
                partition_by,
                order_by,
                output_name: output_name.clone(),
                child: Box::new(child),
            }
        }

        LQueryExpr::BinaryOp {
            op,
            lhs,
            rhs,
            vector_match,
        } => {
            // A binary op's two sides may scan different metrics with different
            // label sets, so each branch must resolve against its OWN bound
            // schema — `convert_root` re-runs the Binder per sub-tree; threading
            // the parent `schema` (a superset over both leaves) would bind the
            // right side's columns to the wrong positions.
            //
            // But an independently-bound side still has to see label names an
            // *enclosing* node references — e.g. an outer `sum by (__name__)` /
            // `sum by (job)` over `(a or b)` whose key is in neither side's own
            // matchers (issue #52). `fallback` carries every name referenced in
            // this scope; subtract the names referenced *within* the binary op
            // itself and what remains is exactly the ancestor-referenced set to
            // inherit — seeding a sibling side's own labels would wrongly leak
            // them across the two branches.
            let own = collect_referenced_columns(legacy);
            let inherited: Vec<String> = inherited_names(fallback)
                .into_iter()
                .filter(|n| !own.contains(n))
                .collect();
            CQueryExpr::BinaryOp {
                op: op.clone(),
                lhs: Box::new(convert_root_with_inherited(lhs, acc, &inherited)?),
                rhs: Box::new(convert_root_with_inherited(rhs, acc, &inherited)?),
                vector_match: vector_match.clone(),
            }
        }
    })
}

/// Wrap resolved group-key ids in the `by`/`without` form (issue #39). PromQL
/// `without(labels)` stores the resolved *excluded* positions; every other
/// grouping is `by`.
fn group_keys(ids: Vec<ColumnId>, without: bool) -> GroupKeys {
    if without {
        GroupKeys::without(ids)
    } else {
        GroupKeys::by(ids)
    }
}

/// The label names an enclosing scope's schema carries beyond the `(ts, value)`
/// floor — the set an independently-bound `BinaryOp` side must inherit so an
/// outer aggregate's group keys still resolve (issue #52).
fn inherited_names(schema: &Schema) -> Vec<String> {
    schema
        .columns
        .iter()
        .filter(|c| c.name != "ts" && c.name != "value")
        .map(|c| c.name.clone())
        .collect()
}

/// Build a canonical `Scan`. A schema-bearing [`SourceSpec`] (SQL table) emits
/// a `Source::Table` carrying that resolved schema; a schema-less one (PromQL)
/// emits a `Source::TimeSeries` carrying the Binder's usage-derived `fallback`.
/// The L2 predicate conjuncts are resolved positionally against the leaf schema.
fn scan(
    spec: &SourceSpec,
    fallback: &Schema,
    pred_conjuncts: &[L2Expr],
) -> Result<CQueryExpr, ConvertError> {
    let (source, schema) = match &spec.schema {
        Some(s) => (
            Source::Table {
                table_ref: spec.name.clone(),
            },
            s.clone(),
        ),
        None => (
            Source::TimeSeries {
                metric: spec.name.clone(),
            },
            fallback.clone(),
        ),
    };
    let predicates = pred_conjuncts
        .iter()
        .map(|e| -> Result<Predicate, ConvertError> { Ok(Predicate(resolve_expr(e, &schema)?)) })
        .collect::<Result<Vec<_>, _>>()?;
    let scan = CQueryExpr::Scan {
        source,
        predicates,
        schema,
    };
    // A non-identity `offset`/`@` lifts into a `TimeShift` wrapper over the scan;
    // an unshifted selector stays a bare `Scan` (issue #40).
    Ok(if spec.shift.is_identity() {
        scan
    } else {
        CQueryExpr::TimeShift {
            shift: spec.shift,
            child: Box::new(scan),
        }
    })
}

/// Resolve a Layer-2 aggregate-input [`ColumnRef`] to a positional input
/// column. `SampleValue` / `Wildcard` carry no specific column → `Ok(None)`
/// (the PromQL sample-value / `COUNT(*)` convention); a `Named` column
/// (`SUM(bytes)`) must resolve to its position, else it is an error — silently
/// dropping it to `None` would reduce the wrong column (the schema probe).
fn resolve_agg_col(col: &ColumnRef, schema: &Schema) -> Result<Option<ColumnId>, ResolveError> {
    match col {
        ColumnRef::Named(name) => {
            schema
                .column_id(name)
                .map(Some)
                .ok_or_else(|| ResolveError::NotFound {
                    name: name.clone(),
                    available: schema.columns.iter().map(|c| c.name.clone()).collect(),
                })
        }
        ColumnRef::Qualified { table, name } => schema
            .column_id_qualified(table, name)
            .or_else(|| schema.column_id(name))
            .map(Some)
            .ok_or_else(|| ResolveError::NotFound {
                name: format!("{table}.{name}"),
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
            }),
        ColumnRef::SampleValue | ColumnRef::Wildcard => Ok(None),
    }
}

/// Map a Layer-2 [`AggFunc`] to its canonical [`AggIntent`], threading the
/// workload's accuracy target onto the approximate intents and the resolved
/// input column (`col`) onto the single-column reducers. `col = None` is the
/// PromQL sample-value convention.
fn agg_func_to_intent(func: &AggFunc, acc: &AccuracyTarget, col: Option<ColumnId>) -> AggIntent {
    match func {
        AggFunc::Count => AggIntent::Count {
            accuracy: acc.clone(),
        },
        AggFunc::Sum => AggIntent::Sum { col },
        AggFunc::Avg => AggIntent::Avg { col },
        AggFunc::Min => AggIntent::Min { col },
        AggFunc::Max => AggIntent::Max { col },
        AggFunc::StdDev { population } => AggIntent::StdDev {
            col,
            population: *population,
        },
        AggFunc::Variance { population } => AggIntent::Variance {
            col,
            population: *population,
        },
        AggFunc::Quantile(q) => AggIntent::Quantile {
            col,
            q: *q,
            accuracy: acc.clone(),
        },
        AggFunc::CountDistinct => AggIntent::Cardinality {
            col,
            accuracy: acc.clone(),
        },
        AggFunc::HeavyHitters { k } => AggIntent::TopK {
            k: *k as usize,
            accuracy: acc.clone(),
        },
        // Range is on the enclosing TimeRange node; intent carries no window.
        AggFunc::Rate { .. } => AggIntent::Rate,
        AggFunc::Increase { .. } => AggIntent::Increase,
        // Counter-derivative range functions (issue #44) — the window rides on
        // the enclosing TimeRange node; scalar params (predict horizon,
        // smoothing factors) are carried in the intent.
        AggFunc::Changes => AggIntent::Changes,
        AggFunc::Delta => AggIntent::Delta,
        AggFunc::IDelta => AggIntent::IDelta,
        AggFunc::Deriv => AggIntent::Deriv,
        AggFunc::Resets => AggIntent::Resets,
        AggFunc::PredictLinear { seconds } => AggIntent::PredictLinear { seconds: *seconds },
        AggFunc::DoubleExpSmoothing { smoothing, trend } => AggIntent::DoubleExpSmoothing {
            smoothing: *smoothing,
            trend: *trend,
        },
        AggFunc::HistogramCount => AggIntent::HistogramCount,
        AggFunc::HistogramSum => AggIntent::HistogramSum,
        AggFunc::HistogramAvg => AggIntent::HistogramAvg,
        AggFunc::HistogramStdDev => AggIntent::HistogramStdDev,
        AggFunc::HistogramStdVar => AggIntent::HistogramStdVar,
        AggFunc::HistogramFraction { lower, upper } => AggIntent::HistogramFraction {
            lower: *lower,
            upper: *upper,
        },
        AggFunc::HistogramQuantile(q) => AggIntent::HistogramQuantile { q: *q },
        AggFunc::Math(m) => AggIntent::Math(m.clone()),
        AggFunc::Absent => AggIntent::Absent,
        AggFunc::AbsentOverTime => AggIntent::AbsentOverTime,
        AggFunc::PresentOverTime => AggIntent::PresentOverTime,
        AggFunc::TimeFn(f) => AggIntent::TimeFn(*f),
        AggFunc::Group => AggIntent::Group,
        AggFunc::CountValues { label } => AggIntent::CountValues {
            label: label.clone(),
        },
        AggFunc::LastOverTime => AggIntent::LastOverTime,
        AggFunc::FirstOverTime => AggIntent::FirstOverTime,
        AggFunc::MadOverTime => AggIntent::MadOverTime,
        AggFunc::TsOfMinOverTime => AggIntent::TsOfMinOverTime,
        AggFunc::TsOfMaxOverTime => AggIntent::TsOfMaxOverTime,
        AggFunc::TsOfFirstOverTime => AggIntent::TsOfFirstOverTime,
        AggFunc::TsOfLastOverTime => AggIntent::TsOfLastOverTime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relational::{AggFunc, AggItem, QueryExpr as LQueryExpr, SourceSpec};
    use asap_ir::intent_algebra::agg_intent::AggIntent;
    use asap_ir::intent_algebra::expr_ir::{CompareOp, L2Expr, L3Expr, L3Scalar};
    use asap_ir::intent_algebra::query_expr::{JoinKind, QueryExpr as CQueryExpr};
    use asap_ir::intent_algebra::schema::{Column, DataType, Schema};

    fn col(name: &str, dtype: DataType) -> Column {
        Column::new(name, dtype, false)
    }

    /// A SQL-shaped `SELECT SUM(bytes), AVG(latency) FROM t` lowers each
    /// reducer onto its own input column (positional), and the derived output
    /// schema types each result off that column (`SUM(bytes:Int64)→Int64`).
    #[test]
    fn counter_derivative_without_window_is_rejected() {
        // A counter-derivative (`Changes`) is per-series over a range and must
        // arrive under an L2 `Window`. If it reaches the converter range-less
        // (no `Window`, and unlike `Rate`/`Increase` it carries no window in its
        // `AggFunc`), emitting it without a `TimeRange` would silently drop its
        // window — the malformed tree is rejected instead (#71).
        let schema = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("value", DataType::Float64),
            ],
            0,
            vec![],
        );
        let tree = LQueryExpr::Aggregate {
            keys: vec![],
            without: false,
            aggs: vec![AggItem {
                alias: None,
                func: AggFunc::Changes,
                col: ColumnRef::SampleValue,
            }],
            having: None,
            input: Box::new(LQueryExpr::Source(SourceSpec::new("m"))), // NO Window
        };
        assert!(matches!(
            convert(&tree, &schema, &AccuracyTarget::Exact),
            Err(ConvertError::RangelessRangeReduction)
        ));
    }

    #[test]
    fn multi_column_aggregate_threads_per_agg_col() {
        let schema = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("bytes", DataType::Int64),
                col("latency", DataType::Float64),
                col("value", DataType::Float64),
            ],
            0,
            vec![],
        );
        let tree = LQueryExpr::Aggregate {
            keys: vec![],
            without: false,
            aggs: vec![
                AggItem {
                    alias: Some("total_bytes".into()),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                },
                AggItem {
                    alias: Some("avg_latency".into()),
                    func: AggFunc::Avg,
                    col: ColumnRef::Named("latency".into()),
                },
            ],
            having: None,
            input: Box::new(LQueryExpr::Source(SourceSpec::new("t"))),
        };

        let l3 = convert(&tree, &schema, &AccuracyTarget::Exact).unwrap();
        let CQueryExpr::Aggregate {
            reduction, aggs, ..
        } = &l3
        else {
            panic!("expected Aggregate, got {l3:?}");
        };
        let Reduction::Reduce(by) = reduction else {
            panic!("expected a Reduce grouping, got {reduction:?}");
        };
        assert!(by.is_empty());
        // bytes is column 1, latency is column 2 in the input schema.
        assert_eq!(
            aggs,
            &vec![
                AggIntent::Sum { col: Some(1) },
                AggIntent::Avg { col: Some(2) },
            ]
        );

        // Output schema types each reducer off its own input column, and names
        // it from the AggItem alias (threaded via Aggregate.output_names).
        let out = l3.output_schema().unwrap();
        assert_eq!(out.columns[0], col("total_bytes", DataType::Int64)); // SUM(bytes:Int64)
        assert_eq!(out.columns[1], col("avg_latency", DataType::Float64)); // AVG(latency)→Float64
    }

    /// PromQL's single sample-value reducer stays `col: None`.
    #[test]
    fn promql_sample_value_agg_stays_col_none() {
        let schema = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("value", DataType::Float64),
            ],
            0,
            vec![],
        );
        let tree = LQueryExpr::Aggregate {
            keys: vec![],
            without: false,
            aggs: vec![AggItem {
                alias: Some("value".into()),
                func: AggFunc::Sum,
                col: ColumnRef::SampleValue,
            }],
            having: None,
            input: Box::new(LQueryExpr::Source(SourceSpec::new("m"))),
        };
        let l3 = convert(&tree, &schema, &AccuracyTarget::Exact).unwrap();
        // single-agg fused path → bare Aggregate (no keys → no Partition)
        let CQueryExpr::Aggregate { aggs, .. } = &l3 else {
            panic!("expected Aggregate, got {l3:?}");
        };
        assert_eq!(aggs, &[AggIntent::Sum { col: None }]);
    }

    /// `SELECT region, SUM(bytes), COUNT(*) FROM logs JOIN meta … GROUP BY region`
    /// — keys and per-agg input columns resolve against the JOIN's *concatenated*
    /// schema, not either leaf's. logs=[id,bytes] meta=[id,region] →
    /// concat [id(0), bytes(1), id(2), region(3)].
    #[test]
    fn aggregate_over_join_resolves_against_concatenated_schema() {
        let logs = LQueryExpr::Source(SourceSpec::with_schema(
            "logs",
            Schema::new(vec![
                col("id", DataType::Int64),
                col("bytes", DataType::Int64),
            ]),
        ));
        let meta = LQueryExpr::Source(SourceSpec::with_schema(
            "meta",
            Schema::new(vec![
                col("id", DataType::Int64),
                col("region", DataType::Utf8),
            ]),
        ));
        let join = LQueryExpr::Join {
            kind: JoinKind::Inner,
            pred: None,
            left: Box::new(logs),
            right: Box::new(meta),
        };
        let tree = LQueryExpr::Aggregate {
            keys: vec![ColumnRef::Named("region".into())],
            without: false,
            aggs: vec![
                AggItem {
                    alias: Some("tot".into()),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                },
                AggItem {
                    alias: Some("n".into()),
                    func: AggFunc::Count,
                    col: ColumnRef::Wildcard,
                },
            ],
            having: None,
            input: Box::new(join),
        };
        let l3 = convert_root(&tree, &AccuracyTarget::Exact).unwrap();
        let CQueryExpr::Aggregate {
            reduction,
            aggs,
            child,
            ..
        } = &l3
        else {
            panic!("expected multi-agg Aggregate, got {l3:?}");
        };
        let Reduction::Reduce(by) = reduction else {
            panic!("expected a Reduce grouping, got {reduction:?}");
        };
        assert_eq!(by, &vec![3], "region is column 3 of the joined schema");
        assert_eq!(
            aggs[0],
            AggIntent::Sum { col: Some(1) },
            "bytes is column 1"
        );
        assert!(matches!(aggs[1], AggIntent::Count { .. }));
        assert!(matches!(child.as_ref(), CQueryExpr::Join { .. }));
    }

    /// `GROUP BY region HAVING <count> > 5` — HAVING references the aggregate
    /// OUTPUT column (`n`), absent from the input schema, so it must resolve
    /// against the derived output schema `[region(0), tot(1), n(2)]`.
    #[test]
    fn having_resolves_against_aggregate_output_schema() {
        let schema = Schema::new(vec![
            col("region", DataType::Utf8),
            col("bytes", DataType::Int64),
        ]);
        let tree = LQueryExpr::Aggregate {
            keys: vec![ColumnRef::Named("region".into())],
            without: false,
            aggs: vec![
                AggItem {
                    alias: Some("tot".into()),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                },
                AggItem {
                    alias: Some("n".into()),
                    func: AggFunc::Count,
                    col: ColumnRef::Wildcard,
                },
            ],
            having: Some(L2Expr::Compare {
                left: Box::new(L2Expr::Column(ColumnRef::Named("n".into()))),
                op: CompareOp::Gt,
                right: Box::new(L2Expr::Literal(L3Scalar::Int64(5))),
            }),
            input: Box::new(LQueryExpr::Source(SourceSpec::with_schema("t", schema))),
        };
        let l3 = convert(&tree, &Schema::default(), &AccuracyTarget::Exact).unwrap();
        let CQueryExpr::Aggregate {
            having: Some(having),
            ..
        } = &l3
        else {
            panic!("expected Aggregate with HAVING, got {l3:?}");
        };
        let L3Expr::Compare { left, .. } = &having.0 else {
            panic!("expected Compare HAVING, got {:?}", having.0);
        };
        assert_eq!(
            **left,
            L3Expr::Column(2),
            "HAVING `n` resolves to the count output column (index 2), not the input schema"
        );
    }
}
