//! Resolve a front-end-emitted, unresolved [`L2QueryExpr`] (`QueryExpr<ColumnRef>`)
//! into the canonical, positional [`L3QueryExpr`] (`QueryExpr<ColumnId>`) —
//! the replacement for [`lower::convert_root`](super::lower::convert_root) for
//! a front end that already builds the canonical shape directly during its
//! own `interpret` step (issue #179), instead of the legacy
//! [`relational`](super::relational) tree.
//!
//! Unlike [`lower::convert`](super::lower::convert), `resolve` does no
//! per-variant *structural* translation — the input is already
//! canonical-shaped, so every [`L2QueryExpr`] variant maps to the identical
//! [`L3QueryExpr`] variant. What's left is exactly the "mechanical,
//! schema-dependent substitution" category #179 describes: a single generic,
//! shape-preserving walk that resolves every [`ColumnRef`] to the
//! [`Binder`](super::binder::Binder)-computed positional [`ColumnId`].

use thiserror::Error;

use super::agg_intent::AggIntent;
use super::binder::Binder;
use super::column_resolution::{
    resolve_column_ref, resolve_column_refs, resolve_expr, resolve_group_keys_promql, ResolveError,
};
use super::expr_ir::ColumnRef;
use super::query_expr::{
    aggregate_output_schema, GroupKeys, L2QueryExpr, L3QueryExpr, Predicate, ProjectItem,
    QueryExprError, Reduction, SortKey,
};
use super::schema::{ColumnId, Schema};

/// Errors from resolving a canonical, unresolved [`L2QueryExpr`] tree.
#[derive(Debug, Error)]
pub enum ResolveTreeError {
    /// A column reference did not resolve against its in-scope schema.
    #[error("column resolution failed: {0}")]
    Resolve(#[from] ResolveError),
    /// Deriving the schema of an already-resolved child failed (needed to
    /// resolve positional column references against it).
    #[error("schema derivation failed: {0}")]
    Schema(#[from] QueryExprError),
}

/// Resolve a whole [`L2QueryExpr`] tree rooted at `tree` into canonical
/// [`L3QueryExpr`]: binds every `ColumnRef` to a `ColumnId` via the
/// [`Binder`], then [`canonicalize`](super::canonicalize::canonicalize)s the
/// result — the same two steps [`convert_root`](super::lower::convert_root)
/// runs, minus the structural-translation step that a front end building this
/// shape directly has already done itself.
pub fn resolve_root(tree: &L2QueryExpr) -> Result<L3QueryExpr, ResolveTreeError> {
    resolve_root_with_inherited(tree, &[])
}

/// [`resolve_root`] with label names inherited from an enclosing scope seeded
/// into the leaf schema — the canonical-tree counterpart to
/// [`convert_root_with_inherited`](super::lower::convert_root), used when
/// re-binding a `BinaryOp` side (issue #52).
fn resolve_root_with_inherited(
    tree: &L2QueryExpr,
    inherited: &[String],
) -> Result<L3QueryExpr, ResolveTreeError> {
    let fallback = Binder::new().bind_query_expr_with_inherited(tree, inherited);
    let l3 = resolve(tree, &fallback)?;
    Ok(super::canonicalize::canonicalize(l3))
}

/// The generic substitution walk: converts children first (bottom-up), then
/// resolves this node's own `ColumnRef`s against the *converted child's*
/// derived output schema — so a `JOIN`'s concatenated schema and a cross-
/// series aggregate's frozen-closed output bind to the right positions, same
/// as [`lower::convert`](super::lower::convert).
fn resolve(tree: &L2QueryExpr, fallback: &Schema) -> Result<L3QueryExpr, ResolveTreeError> {
    use super::query_expr::QueryExpr as QE;
    Ok(match tree {
        QE::Scan {
            source,
            predicates,
            schema,
        } => {
            let schema = schema.clone().unwrap_or_else(|| fallback.clone());
            let predicates = predicates
                .iter()
                .map(|Predicate(e)| Ok(Predicate(resolve_expr(e, &schema)?)))
                .collect::<Result<Vec<_>, ResolveError>>()?;
            QE::Scan {
                source: source.clone(),
                predicates,
                schema,
            }
        }

        QE::Scalar(v) => QE::Scalar(*v),
        QE::EvalTime => QE::EvalTime,

        QE::VectorFromScalar(child) => QE::VectorFromScalar(Box::new(resolve(child, fallback)?)),
        QE::ScalarFromVector(child) => QE::ScalarFromVector(Box::new(resolve(child, fallback)?)),

        QE::Relabel { dst, value, child } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            QE::Relabel {
                dst: dst.clone(),
                value: resolve_expr(value, &child_schema)?,
                child: Box::new(child),
            }
        }

        QE::InfoJoin { selector, child } => QE::InfoJoin {
            selector: selector.clone(),
            child: Box::new(resolve(child, fallback)?),
        },

        QE::Sample { by, kind, child } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            QE::Sample {
                by: resolve_group_keys(by, &child_schema)?,
                kind: *kind,
                child: Box::new(child),
            }
        }

        QE::Filter { pred, child } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            QE::Filter {
                pred: Predicate(resolve_expr(&pred.0, &child_schema)?),
                child: Box::new(child),
            }
        }

        QE::Project {
            cols,
            qualifier,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            let cols = cols
                .iter()
                .map(|item| -> Result<ProjectItem, ResolveError> {
                    Ok(ProjectItem {
                        alias: item.alias.clone(),
                        expr: resolve_expr(&item.expr, &child_schema)?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            QE::Project {
                cols,
                qualifier: qualifier.clone(),
                child: Box::new(child),
            }
        }

        QE::Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            let reduction = resolve_reduction(reduction, &child_schema)?;
            let measures = measures
                .iter()
                .map(|m| resolve_agg_intent(m, &child_schema))
                .collect::<Result<Vec<_>, ResolveError>>()?;
            let having = having
                .as_ref()
                .map(|Predicate(h)| -> Result<Predicate, ResolveTreeError> {
                    let out_schema =
                        aggregate_output_schema(&child_schema, &reduction, &measures, output_names)?;
                    Ok(Predicate(resolve_expr(h, &out_schema)?))
                })
                .transpose()?;
            QE::Aggregate {
                reduction,
                measures,
                output_names: output_names.clone(),
                having,
                child: Box::new(child),
            }
        }

        QE::Distinct { cols, child } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            QE::Distinct {
                cols: resolve_column_refs(cols, &child_schema)?,
                child: Box::new(child),
            }
        }

        QE::Merge { children } => QE::Merge {
            children: children
                .iter()
                .map(|c| resolve(c, fallback))
                .collect::<Result<Vec<_>, _>>()?,
        },

        QE::Join {
            kind,
            pred,
            left,
            right,
        } => {
            // Each branch is bound independently, same reasoning as `BinaryOp`
            // below — different leaves / label sets.
            let left = resolve_root_with_inherited(left, &[])?;
            let right = resolve_root_with_inherited(right, &[])?;
            let mut concat = left.output_schema()?;
            concat.columns.extend(right.output_schema()?.columns);
            let pred = Predicate(resolve_expr(&pred.0, &concat)?);
            QE::Join {
                kind: kind.clone(),
                pred,
                left: Box::new(left),
                right: Box::new(right),
            }
        }

        QE::SetOp {
            kind,
            all,
            left,
            right,
        } => QE::SetOp {
            kind: kind.clone(),
            all: *all,
            left: Box::new(resolve_root_with_inherited(left, &[])?),
            right: Box::new(resolve_root_with_inherited(right, &[])?),
        },

        QE::Sort {
            keys,
            partition_by,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            let keys = keys
                .iter()
                .map(|k| -> Result<SortKey, ResolveError> {
                    Ok(SortKey {
                        expr: resolve_expr(&k.expr, &child_schema)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let partition_by = resolve_group_keys(partition_by, &child_schema)?;
            QE::Sort {
                keys,
                partition_by,
                child: Box::new(child),
            }
        }

        QE::Limit { n, offset, child } => QE::Limit {
            n: *n,
            offset: *offset,
            child: Box::new(resolve(child, fallback)?),
        },

        QE::Subquery {
            range,
            resolution,
            child,
        } => QE::Subquery {
            range: *range,
            resolution: *resolution,
            child: Box::new(resolve(child, fallback)?),
        },

        QE::TimeRange { range, child } => QE::TimeRange {
            range: *range,
            child: Box::new(resolve(child, fallback)?),
        },

        QE::TimeShift { shift, child } => QE::TimeShift {
            shift: *shift,
            child: Box::new(resolve(child, fallback)?),
        },

        QE::WindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let child_schema = child.output_schema()?;
            let args = args
                .iter()
                .map(|a| resolve_expr(a, &child_schema))
                .collect::<Result<Vec<_>, _>>()?;
            let partition_by = resolve_group_keys(partition_by, &child_schema)?;
            let order_by = order_by
                .iter()
                .map(|k| -> Result<SortKey, ResolveError> {
                    Ok(SortKey {
                        expr: resolve_expr(&k.expr, &child_schema)?,
                        ascending: k.ascending,
                        nulls_first: k.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            QE::WindowFunc {
                func: func.clone(),
                args,
                partition_by,
                order_by,
                output_name: output_name.clone(),
                child: Box::new(child),
            }
        }

        QE::BinaryOp {
            op,
            lhs,
            rhs,
            vector_match,
        } => {
            // A binary op's two sides may scan different metrics with
            // different label sets, so each branch resolves against its OWN
            // bound schema; but an independently-bound side still has to see
            // label names an *enclosing* node references (issue #52) — same
            // reasoning as `lower::convert`'s `BinaryOp` arm.
            let own = super::binder::collect_referenced_columns_qe(tree);
            let inherited: Vec<String> = inherited_names(fallback)
                .into_iter()
                .filter(|n| !own.contains(n))
                .collect();
            QE::BinaryOp {
                op: op.clone(),
                lhs: Box::new(resolve_root_with_inherited(lhs, &inherited)?),
                rhs: Box::new(resolve_root_with_inherited(rhs, &inherited)?),
                vector_match: vector_match.clone(),
            }
        }
    })
}

/// The label names an enclosing scope's schema carries beyond the `(ts,
/// value)` floor — mirrors [`lower::inherited_names`](super::lower).
fn inherited_names(schema: &Schema) -> Vec<String> {
    schema
        .columns
        .iter()
        .filter(|c| c.name != "ts" && c.name != "value")
        .map(|c| c.name.clone())
        .collect()
}

/// Resolve a name-based [`GroupKeys<ColumnRef>`] into positional
/// [`GroupKeys<ColumnId>`], preserving its `by`/`without` mode.
fn resolve_group_keys(
    keys: &GroupKeys<ColumnRef>,
    schema: &Schema,
) -> Result<GroupKeys<ColumnId>, ResolveError> {
    let ids = resolve_column_refs(keys.keys(), schema)?;
    Ok(if keys.is_without() {
        GroupKeys::without(ids)
    } else {
        GroupKeys::by(ids)
    })
}

/// Resolve a name-based [`Reduction<ColumnRef>`] into positional
/// [`Reduction<ColumnId>`].
///
/// Uses [`resolve_group_keys_promql`] rather than the strict
/// [`resolve_group_keys`], unlike every other group-key site in `resolve`
/// (`Sample.by`, `Sort.partition_by`, `WindowFunc.partition_by`): a key
/// absent from a **closed** schema (e.g. the output of a nested cross-series
/// aggregate that collapsed the label) is provably absent from every row, so
/// PromQL drops it from the grouping rather than rejecting the query (issue
/// #53) — `sum(sum by (group) (m)) by (job)` is the canonical case, `job`
/// absent from the inner aggregate's closed `[group, sum]` output. Mirrors
/// `lower::convert`'s per-series-fused `Aggregate` branch, the only one a
/// front end building this shape directly ever takes (every `Aggregate` it
/// builds is single-measure and HAVING-less) — see
/// `asap_frontend_promql::promql::reduction_for`.
fn resolve_reduction(
    reduction: &Reduction<ColumnRef>,
    schema: &Schema,
) -> Result<Reduction<ColumnId>, ResolveError> {
    Ok(match reduction {
        Reduction::Reduce(by) => {
            let ids = resolve_group_keys_promql(by.keys(), schema)?;
            Reduction::Reduce(if by.is_without() {
                GroupKeys::without(ids)
            } else {
                GroupKeys::by(ids)
            })
        }
        Reduction::PerEntity => Reduction::PerEntity,
    })
}

/// Resolve a name-based [`AggIntent<ColumnRef>`] into positional
/// [`AggIntent<ColumnId>`] — every `col: Option<ColumnRef>` resolves to
/// `Option<ColumnId>` (`None` stays `None`, the sample-value convention);
/// every other field carries straight through unchanged.
fn resolve_agg_intent(
    intent: &AggIntent<ColumnRef>,
    schema: &Schema,
) -> Result<AggIntent<ColumnId>, ResolveError> {
    let col = |c: &Option<ColumnRef>| -> Result<Option<ColumnId>, ResolveError> {
        c.as_ref().map(|r| resolve_column_ref(r, schema)).transpose()
    };
    Ok(match intent {
        AggIntent::Count { accuracy } => AggIntent::Count {
            accuracy: accuracy.clone(),
        },
        AggIntent::Sum { col: c } => AggIntent::Sum { col: col(c)? },
        AggIntent::Min { col: c } => AggIntent::Min { col: col(c)? },
        AggIntent::Max { col: c } => AggIntent::Max { col: col(c)? },
        AggIntent::Avg { col: c } => AggIntent::Avg { col: col(c)? },
        AggIntent::StdDev { col: c, population } => AggIntent::StdDev {
            col: col(c)?,
            population: *population,
        },
        AggIntent::Variance { col: c, population } => AggIntent::Variance {
            col: col(c)?,
            population: *population,
        },
        AggIntent::Quantile { col: c, q, accuracy } => AggIntent::Quantile {
            col: col(c)?,
            q: *q,
            accuracy: accuracy.clone(),
        },
        AggIntent::TopK { k, accuracy } => AggIntent::TopK {
            k: *k,
            accuracy: accuracy.clone(),
        },
        AggIntent::Cardinality { col: c, accuracy } => AggIntent::Cardinality {
            col: col(c)?,
            accuracy: accuracy.clone(),
        },
        AggIntent::Rate => AggIntent::Rate,
        AggIntent::Increase => AggIntent::Increase,
        AggIntent::Changes => AggIntent::Changes,
        AggIntent::Delta => AggIntent::Delta,
        AggIntent::IDelta => AggIntent::IDelta,
        AggIntent::Deriv => AggIntent::Deriv,
        AggIntent::Resets => AggIntent::Resets,
        AggIntent::PredictLinear { seconds } => AggIntent::PredictLinear { seconds: *seconds },
        AggIntent::DoubleExpSmoothing { smoothing, trend } => AggIntent::DoubleExpSmoothing {
            smoothing: *smoothing,
            trend: *trend,
        },
        AggIntent::HistogramCount => AggIntent::HistogramCount,
        AggIntent::HistogramSum => AggIntent::HistogramSum,
        AggIntent::HistogramAvg => AggIntent::HistogramAvg,
        AggIntent::HistogramStdDev => AggIntent::HistogramStdDev,
        AggIntent::HistogramStdVar => AggIntent::HistogramStdVar,
        AggIntent::HistogramFraction { lower, upper } => AggIntent::HistogramFraction {
            lower: *lower,
            upper: *upper,
        },
        AggIntent::HistogramQuantile { q } => AggIntent::HistogramQuantile { q: *q },
        AggIntent::Math(f) => AggIntent::Math(f.clone()),
        AggIntent::Absent => AggIntent::Absent,
        AggIntent::AbsentOverTime => AggIntent::AbsentOverTime,
        AggIntent::PresentOverTime => AggIntent::PresentOverTime,
        AggIntent::TimeFn(f) => AggIntent::TimeFn(*f),
        AggIntent::Group => AggIntent::Group,
        AggIntent::CountValues { label } => AggIntent::CountValues {
            label: label.clone(),
        },
        AggIntent::LastOverTime => AggIntent::LastOverTime,
        AggIntent::FirstOverTime => AggIntent::FirstOverTime,
        AggIntent::MadOverTime => AggIntent::MadOverTime,
        AggIntent::TsOfMinOverTime => AggIntent::TsOfMinOverTime,
        AggIntent::TsOfMaxOverTime => AggIntent::TsOfMaxOverTime,
        AggIntent::TsOfFirstOverTime => AggIntent::TsOfFirstOverTime,
        AggIntent::TsOfLastOverTime => AggIntent::TsOfLastOverTime,
        AggIntent::Extension { ext_kind, payload } => AggIntent::Extension {
            ext_kind: ext_kind.clone(),
            payload: payload.clone(),
        },
    })
}
