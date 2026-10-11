//! Resolve a front-end-emitted [`UnresolvedOp`] tree into the unified IR
//! ([`Rc<OperatorNode>`]): a single, shape-preserving, bottom-up walk that
//! binds every [`ColumnRef`] to a positional `ColumnId`.
//!
//! Every structural decision (reduction choice, window folds, heavy-hitter
//! recognition, ...) is the front end's; what is left here is the mechanical,
//! schema-dependent substitution. Children are resolved first; each child
//! becomes an `OperatorNode` whose derived `.schema` is the scope the parent's
//! own references resolve against, so a `JOIN`'s concatenated schema and a
//! cross-series aggregate's frozen-closed output bind to the right positions.
//!
//! Scope boundaries: `Join` / `SetOp` sides and the operators referenced from
//! scalar positions (`scalar(v)`, subqueries) are each bound as a root in
//! their own scope. A `BinaryOp` side is too, but additionally inherits the
//! label names its enclosing scope references (issue #52): the `job` in
//! `sum by (job)(a or b)` appears in neither side's own matchers.

use asap_types::ir::schema::aggregate_schema::aggregate_output_schema;
use std::rc::Rc;

use thiserror::Error;

use asap_types::ir::operator::operator_properties::ConcatDiscriminatorKey;
use asap_types::ir::operator::{AggIntent, GroupKeys, Reduction};
use asap_types::ir::scalar::column_resolution::resolve_group_keys_promql;
use asap_types::ir::scalar::{resolve_column_ref, resolve_column_refs, ColumnRef, ResolveError};
use asap_types::ir::schema::{ColumnId, Schema, SchemaDerivationError};
use asap_types::ir::{NonASAPOp, OperatorNode, Predicate, ProjectItem, ScalarExpr, SortKey};

use crate::schema_resolver::{collect_referenced_columns, SchemaResolver};
use crate::unresolved::{UnresolvedOp, UnresolvedScalar, UnresolvedSortKey};

/// Errors from resolving an [`UnresolvedOp`] tree.
#[derive(Debug, Error)]
pub enum ResolveDAGError {
    /// A column reference did not resolve against its in-scope schema.
    #[error("column resolution failed: {0}")]
    Resolve(#[from] ResolveError),
    /// Deriving the schema of an already-resolved child failed (needed to
    /// resolve positional column references against it).
    #[error("schema derivation failed: {0}")]
    Schema(#[from] SchemaDerivationError),
}

use asap_types::ir::canonicalize::canonicalize;

/// Resolve the whole tree rooted at `tree`: bind every `ColumnRef` to a
/// `ColumnId` via the [`SchemaResolver`], then canonicalize the result.
pub fn resolve_root(tree: &UnresolvedOp) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    resolve_root_with_inherited(tree, &[])
}

/// [`resolve_root`] with label names inherited from an enclosing scope seeded
/// into the leaf schema (a `BinaryOp` side, a scalar operand's operator).
fn resolve_root_with_inherited(
    tree: &UnresolvedOp,
    inherited: &[String],
) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    let fallback = SchemaResolver::new().resolve_schema_with_inherited(tree, inherited);
    let root = resolve(tree, &fallback)?;
    let root = canonicalize(root)?;
    root.validate_structure()?;
    Ok(root)
}

/// Bind `tree` as a root in its own scope, inheriting from `enclosing` the
/// label names `tree` does not reference itself (issue #52).
fn resolve_nested_root(
    tree: &UnresolvedOp,
    enclosing: &Schema,
) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    let own = collect_referenced_columns(tree);
    let inherited: Vec<String> = inherited_names(enclosing)
        .into_iter()
        .filter(|n| !own.contains(n))
        .collect();
    resolve_root_with_inherited(tree, &inherited)
}

fn node(op: NonASAPOp) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    Ok(OperatorNode::new_shared(
        asap_types::ir::Operator::NonASAP(op),
    )?)
}

/// The generic substitution walk. `fallback` is the usage-derived schema a
/// schemaless `Scan` in this scope binds to.
fn resolve(tree: &UnresolvedOp, fallback: &Schema) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    use UnresolvedOp as U;
    let expr = |e: &UnresolvedScalar, schema: &Schema| resolve_expr_in(e, schema, fallback);
    let pred = |p: &UnresolvedScalar, schema: &Schema| {
        Ok::<_, ResolveDAGError>(Predicate(expr(p, schema)?))
    };
    let sort_keys = |keys: &[UnresolvedSortKey], schema: &Schema| {
        keys.iter()
            .map(|k| {
                Ok::<_, ResolveDAGError>(SortKey {
                    expr: expr(&k.expr, schema)?,
                    ascending: k.ascending,
                    nulls_first: k.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, _>>()
    };
    match tree {
        U::Scan {
            source,
            predicates,
            schema,
        } => {
            let schema = schema.clone().unwrap_or_else(|| fallback.clone());
            let predicates = predicates
                .iter()
                .map(|p| pred(&p.0, &schema))
                .collect::<Result<Vec<_>, _>>()?;
            node(NonASAPOp::Scan {
                source: source.clone(),
                predicates,
                schema,
            })
        }

        // Row expressions have no input-column scope.
        U::Values { rows, schema } => {
            let empty = Schema::new(Vec::new());
            let rows = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|e| expr(e, &empty))
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            node(NonASAPOp::Values {
                rows,
                schema: schema.clone(),
            })
        }

        // A scalar at an operator position has no child scope; in practice a
        // literal, so `fallback` is never consulted for a column here.
        U::PromqlScalarOp {
            child,
            scalar,
            op,
            scalar_left,
            return_bool,
        } => {
            let child = resolve(child, fallback)?;
            let child = if child.schema.closed {
                child
            } else {
                asap_types::ir::schema_support::with_promql_series_identity(&child)
                    .map_err(SchemaDerivationError::InvalidScalarSignature)?
            };
            let scalar = resolve_expr(scalar, &Schema::default())?;
            lower_scalar_vector(child, scalar, op, *scalar_left, *return_bool)
        }
        U::PromqlMap {
            child,
            sample,
            drop_metric_name,
        } => {
            let child = resolve(child, fallback)?;
            let child = if child.schema.closed {
                child
            } else {
                asap_types::ir::schema_support::with_promql_series_identity(&child)
                    .map_err(SchemaDerivationError::InvalidScalarSignature)?
            };
            let sample = resolve_expr(sample, &child.schema)?;
            project_sample(child, sample, *drop_metric_name)
        }
        U::PromqlVectorFromScalar(inner) => {
            node(NonASAPOp::PromqlVectorFromScalar(expr(inner, fallback)?))
        }

        U::PromqlRelabel { dst, value, child } => {
            let child = resolve(child, fallback)?;
            let value = expr(value, &child.schema)?;
            node(NonASAPOp::PromqlRelabel {
                dst: dst.clone(),
                value,
                child,
            })
        }

        U::PromqlInfoEnrich { selector, child } => node(NonASAPOp::PromqlInfoEnrich {
            selector: selector.clone(),
            child: resolve(child, fallback)?,
        }),

        U::PromqlSeriesSample { by, kind, child } => {
            let child = resolve(child, fallback)?;
            let by = resolve_group_keys(by, &child.schema)?;
            node(NonASAPOp::PromqlSeriesSample {
                by,
                kind: *kind,
                child,
            })
        }

        U::Filter { pred: p, child } => {
            let child = resolve(child, fallback)?;
            let pred = pred(&p.0, &child.schema)?;
            node(NonASAPOp::Filter { pred, child })
        }

        U::Project {
            cols,
            qualifier,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let cols = cols
                .iter()
                .map(|item| {
                    Ok::<_, ResolveDAGError>(ProjectItem {
                        alias: item.alias.clone(),
                        expr: expr(&item.expr, &child.schema)?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            node(NonASAPOp::Project {
                cols,
                qualifier: qualifier.clone(),
                child,
            })
        }

        U::Aggregate {
            reduction,
            measures,
            output_names,
            filters,
            having,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let reduction = resolve_reduction(reduction, &child.schema)?;
            let measures = measures
                .iter()
                .map(|m| resolve_agg_intent(m, &child.schema))
                .collect::<Result<Vec<_>, ResolveError>>()?;
            let filters = filters
                .iter()
                .map(|p| p.as_ref().map(|p| pred(&p.0, &child.schema)).transpose())
                .collect::<Result<Vec<_>, _>>()?;
            // HAVING is evaluated over the aggregate's own output.
            let having = having
                .as_ref()
                .map(|h| {
                    let out_schema = aggregate_output_schema(
                        &child.schema,
                        &reduction,
                        &measures,
                        output_names,
                    )?;
                    pred(&h.0, &out_schema)
                })
                .transpose()?;
            node(NonASAPOp::Aggregate {
                reduction,
                measures,
                output_names: output_names.clone(),
                filters,
                having,
                child,
            })
        }

        U::Dedup { cols, child } => {
            let child = resolve(child, fallback)?;
            let cols = resolve_column_refs(cols, &child.schema)?;
            node(NonASAPOp::Dedup { cols, child })
        }

        U::Concat {
            children,
            discriminator_unique_key,
        } => {
            let children = children
                .iter()
                .map(|c| resolve(c, fallback))
                .collect::<Result<Vec<_>, _>>()?;
            // Resolved against the first branch's own output schema — the one
            // `output_schema`'s `Concat` arm derives the merged schema from.
            let discriminator_unique_key = discriminator_unique_key
                .as_ref()
                .map(|key| {
                    let schema = &children
                        .first()
                        .ok_or(SchemaDerivationError::EmptyConcat)?
                        .schema;
                    Ok::<_, ResolveDAGError>(ConcatDiscriminatorKey::new(
                        resolve_column_ref(key.discriminator(), schema)?,
                        resolve_column_refs(key.inner_key(), schema)?,
                    ))
                })
                .transpose()?;
            node(NonASAPOp::Concat {
                children,
                discriminator_unique_key,
            })
        }

        U::Join {
            kind,
            pred: p,
            left,
            right,
        } => {
            // Each branch is bound independently (different leaves / label
            // sets); the predicate sees left ++ right.
            let left = resolve_root_with_inherited(left, &[])?;
            let right = resolve_root_with_inherited(right, &[])?;
            let mut concat = left.schema.clone();
            concat.fields.extend(right.schema.fields.iter().cloned());
            let pred = pred(&p.0, &concat)?;
            node(NonASAPOp::Join {
                kind: kind.clone(),
                pred,
                left,
                right,
            })
        }

        U::SetOp {
            kind,
            all,
            left,
            right,
        } => node(NonASAPOp::SetOp {
            kind: kind.clone(),
            all: *all,
            left: resolve_root_with_inherited(left, &[])?,
            right: resolve_root_with_inherited(right, &[])?,
        }),

        U::Sort {
            keys,
            partition_by,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let keys = sort_keys(keys, &child.schema)?;
            let partition_by = resolve_group_keys(partition_by, &child.schema)?;
            node(NonASAPOp::Sort {
                keys,
                partition_by,
                child,
            })
        }

        U::Limit {
            n,
            offset,
            partition_by,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let partition_by = resolve_group_keys(partition_by, &child.schema)?;
            node(NonASAPOp::Limit {
                n: *n,
                offset: *offset,
                partition_by,
                child,
            })
        }

        U::PromqlSubquery {
            range,
            resolution,
            child,
        } => node(NonASAPOp::PromqlSubquery {
            range: *range,
            resolution: *resolution,
            child: resolve(child, fallback)?,
        }),

        U::TimeRange { range, kind, child } => node(NonASAPOp::TimeRange {
            range: *range,
            kind: *kind,
            child: resolve(child, fallback)?,
        }),

        U::TimeShift { shift, child } => node(NonASAPOp::TimeShift {
            shift: *shift,
            child: resolve(child, fallback)?,
        }),

        U::SQLWindowFunc {
            func,
            args,
            partition_by,
            order_by,
            frame,
            output_name,
            child,
        } => {
            let child = resolve(child, fallback)?;
            let args = args
                .iter()
                .map(|a| expr(a, &child.schema))
                .collect::<Result<Vec<_>, _>>()?;
            let partition_by = resolve_group_keys(partition_by, &child.schema)?;
            let order_by = sort_keys(order_by, &child.schema)?;
            node(NonASAPOp::SQLWindowFunc {
                func: func.clone(),
                args,
                partition_by,
                order_by,
                frame: frame.clone(),
                output_name: output_name.clone(),
                child,
            })
        }

        U::BinaryOp {
            operator,
            return_bool,
            lhs,
            rhs,
        } => {
            // The two sides may scan different metrics with different label
            // sets, so each resolves against its OWN bound schema — but still
            // sees the label names the enclosing scope references (issue #52).
            // The inherited set is computed over the whole `BinaryOp`, so one
            // side's own labels are not conjured into the other.
            let own = collect_referenced_columns(tree);
            let inherited: Vec<String> = inherited_names(fallback)
                .into_iter()
                .filter(|n| !own.contains(n))
                .collect();
            node(NonASAPOp::BinaryOp {
                operator: operator.clone(),
                return_bool: *return_bool,
                lhs: resolve_root_with_inherited(lhs, &inherited)?,
                rhs: resolve_root_with_inherited(rhs, &inherited)?,
            })
        }
    }
}

/// The label names an enclosing scope's schema carries beyond the `(ts,
/// value)` floor.
fn inherited_names(schema: &Schema) -> Vec<String> {
    schema
        .fields
        .iter()
        .filter(|c| c.name != "ts" && c.name != "value")
        .map(|c| c.name.clone())
        .collect()
}

/// Resolve a name-based scalar expression against `schema`. Operators it
/// reads (`scalar(v)`, subqueries) are bound as roots in their own scope,
/// inheriting `schema`'s label names.
pub fn resolve_expr(
    expr: &UnresolvedScalar,
    schema: &Schema,
) -> Result<ScalarExpr, ResolveDAGError> {
    resolve_expr_in(expr, schema, schema)
}

/// [`resolve_expr`] where the operators the expression reads inherit from
/// `enclosing` (the owning root's fallback schema) rather than from `schema`.
fn resolve_expr_in(
    expr: &UnresolvedScalar,
    schema: &Schema,
    enclosing: &Schema,
) -> Result<ScalarExpr, ResolveDAGError> {
    use UnresolvedScalar as S;
    let bx = |e: &UnresolvedScalar| -> Result<Box<ScalarExpr>, ResolveDAGError> {
        Ok(Box::new(resolve_expr_in(e, schema, enclosing)?))
    };
    let each = |es: &[UnresolvedScalar]| -> Result<Vec<ScalarExpr>, ResolveDAGError> {
        es.iter()
            .map(|e| resolve_expr_in(e, schema, enclosing))
            .collect()
    };
    let op = |o: &UnresolvedOp| resolve_nested_root(o, enclosing);
    Ok(match expr {
        S::Column(c) => ScalarExpr::Column(resolve_column_ref(c, schema)?),
        S::Literal(s) => ScalarExpr::Literal(s.clone()),
        S::EvalTimestamp => ScalarExpr::EvalTimestamp,
        S::CurrentTimestamp => ScalarExpr::CurrentTimestamp,
        S::Negative { expr, semantics } => ScalarExpr::Negative {
            expr: bx(expr)?,
            semantics: *semantics,
        },
        S::Compare {
            left,
            op,
            right,
            semantics,
        } => ScalarExpr::Compare {
            left: bx(left)?,
            op: op.clone(),
            right: bx(right)?,
            semantics: *semantics,
        },
        S::BoolAnd(v) => ScalarExpr::BoolAnd(each(v)?),
        S::BoolOr(v) => ScalarExpr::BoolOr(each(v)?),
        S::Not(e) => ScalarExpr::Not(bx(e)?),
        S::IsNull(e) => ScalarExpr::IsNull(bx(e)?),
        S::IsNotNull(e) => ScalarExpr::IsNotNull(bx(e)?),
        S::Cast { expr, to, try_cast } => ScalarExpr::Cast {
            expr: bx(expr)?,
            to: to.clone(),
            try_cast: *try_cast,
        },
        S::InList {
            expr,
            list,
            negated,
        } => ScalarExpr::InList {
            expr: bx(expr)?,
            list: each(list)?,
            negated: *negated,
        },
        S::FunctionCall { name, args } => ScalarExpr::FunctionCall {
            name: name.clone(),
            args: each(args)?,
        },
        S::Arithmetic {
            op,
            left,
            right,
            semantics,
        } => ScalarExpr::Arithmetic {
            op: op.clone(),
            left: bx(left)?,
            right: bx(right)?,
            semantics: *semantics,
        },
        S::Case {
            operand,
            branches,
            else_expr,
        } => ScalarExpr::Case {
            operand: operand.as_deref().map(bx).transpose()?,
            branches: branches
                .iter()
                .map(|(w, t)| {
                    Ok((
                        resolve_expr_in(w, schema, enclosing)?,
                        resolve_expr_in(t, schema, enclosing)?,
                    ))
                })
                .collect::<Result<Vec<_>, ResolveDAGError>>()?,
            else_expr: else_expr.as_deref().map(bx).transpose()?,
        },
        S::PromqlScalarFromVector(o) => ScalarExpr::PromqlScalarFromVector(op(o)?),
        S::ScalarSubquery(o) => ScalarExpr::ScalarSubquery(op(o)?),
        S::Exists { subquery, negated } => ScalarExpr::Exists {
            subquery: op(subquery)?,
            negated: *negated,
        },
        S::InSubquery {
            expr,
            subquery,
            negated,
        } => ScalarExpr::InSubquery {
            expr: bx(expr)?,
            subquery: op(subquery)?,
            negated: *negated,
        },
    })
}

/// Resolve name-based group keys positionally, preserving `by`/`without`.
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

/// Resolve a name-based reduction. Uses [`resolve_group_keys_promql`] rather
/// than the strict [`resolve_group_keys`]: a key absent from a **closed**
/// schema (the output of a nested cross-series aggregate that collapsed the
/// label) is provably absent from every row, so PromQL drops it from the
/// grouping rather than rejecting the query (issue #53) — `sum(sum by (group)
/// (m)) by (job)`. SQL `GROUP BY` keys are always present, so the lenient
/// path is a no-op difference there.
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

/// Resolve a name-based aggregate intent: every `col: Option<ColumnRef>`
/// resolves to `Option<ColumnId>` (`None` stays `None`, the sample-value
/// convention); every other field carries through unchanged.
fn resolve_agg_intent(
    intent: &AggIntent<ColumnRef>,
    schema: &Schema,
) -> Result<AggIntent<ColumnId>, ResolveError> {
    let col = |c: &Option<ColumnRef>| -> Result<Option<ColumnId>, ResolveError> {
        c.as_ref()
            .map(|r| resolve_column_ref(r, schema))
            .transpose()
    };
    Ok(match intent {
        AggIntent::Count { accuracy } => AggIntent::Count {
            accuracy: accuracy.clone(),
        },
        AggIntent::PearsonCorr { left, right } => AggIntent::PearsonCorr {
            left: resolve_column_ref(left, schema)?,
            right: resolve_column_ref(right, schema)?,
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
        AggIntent::Quantile {
            col: c,
            q,
            accuracy,
        } => AggIntent::Quantile {
            col: col(c)?,
            q: *q,
            accuracy: accuracy.clone(),
        },
        AggIntent::TopK { k, accuracy } => AggIntent::TopK {
            k: *k,
            accuracy: accuracy.clone(),
        },
        AggIntent::Cardinality { cols, accuracy } => AggIntent::Cardinality {
            cols: cols
                .iter()
                .map(|c| resolve_column_ref(c, schema))
                .collect::<Result<_, _>>()?,
            accuracy: accuracy.clone(),
        },
        AggIntent::FrequencyL2 { col: c, accuracy } => AggIntent::FrequencyL2 {
            col: col(c)?,
            accuracy: accuracy.clone(),
        },
        AggIntent::FrequencyEntropy { col: c, accuracy } => AggIntent::FrequencyEntropy {
            col: col(c)?,
            accuracy: accuracy.clone(),
        },
        AggIntent::Rate => AggIntent::Rate,
        AggIntent::IRate => AggIntent::IRate,
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
        AggIntent::HistogramQuantile { q, le } => AggIntent::HistogramQuantile {
            q: *q,
            le: resolve_column_ref(le, schema)?,
        },
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

/// Resolve a standalone scalar in an empty column scope; plan reads retain their own scope.
pub fn resolve_scalar_root(tree: &UnresolvedScalar) -> Result<ScalarExpr, ResolveDAGError> {
    let resolved = resolve_expr(tree, &Schema::default())?;
    resolved.scalar_type(&Schema::default())?;
    Ok(resolved)
}

fn lower_scalar_vector(
    child: Rc<OperatorNode>,
    scalar: ScalarExpr,
    op: &asap_types::ir::operator::BinaryOpKind,
    scalar_left: bool,
    return_bool: bool,
) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    use asap_types::ir::operator::BinaryOpKind;
    use asap_types::ir::scalar::ScalarValue;
    use asap_types::ir::schema::DataType;
    use asap_types::ir::ExprSemantics;
    let value = child
        .schema
        .column_id("value")
        .or_else(|| {
            child
                .schema
                .fields
                .iter()
                .enumerate()
                .filter(|(i, f)| {
                    Some(*i) != child.schema.time_index
                        && matches!(f.plain_dtype(), Some(DataType::Float64 | DataType::Int64))
                })
                .map(|(i, _)| i)
                .next_back()
        })
        .ok_or_else(|| {
            SchemaDerivationError::InvalidScalarSignature("vector has no numeric sample".into())
        })?;
    let sample = ScalarExpr::Column(value);
    let (left, right) = if scalar_left {
        (scalar, sample)
    } else {
        (sample, scalar)
    };
    let semantics = ExprSemantics::Promql;
    let return_bool = return_bool || matches!(op, BinaryOpKind::CompareBool(_));
    let computed = match op {
        BinaryOpKind::Arithmetic(op) => ScalarExpr::Arithmetic {
            op: op.clone(),
            left: Box::new(left),
            right: Box::new(right),
            semantics,
        },
        BinaryOpKind::Compare(op) | BinaryOpKind::CompareBool(op) => {
            let predicate = ScalarExpr::Compare {
                op: op.clone(),
                left: Box::new(left),
                right: Box::new(right),
                semantics,
            };
            if !return_bool {
                return node(NonASAPOp::Filter {
                    child,
                    pred: Predicate(predicate),
                });
            }
            ScalarExpr::Case {
                operand: None,
                branches: vec![(predicate, ScalarExpr::Literal(ScalarValue::Float64(1.0)))],
                else_expr: Some(Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.0)))),
            }
        }
        BinaryOpKind::Set(_) => {
            return Err(SchemaDerivationError::InvalidScalarSignature(
                "set operators require two vectors".into(),
            )
            .into())
        }
    };
    project_sample(child, computed, true)
}

fn project_sample(
    child: Rc<OperatorNode>,
    computed: ScalarExpr,
    drop_metric_name: bool,
) -> Result<Rc<OperatorNode>, ResolveDAGError> {
    let value = asap_types::ir::scalar::column_resolution::resolve_column_ref(
        &ColumnRef::SampleValue,
        &child.schema,
    )?;
    let cols = child
        .schema
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !drop_metric_name || f.name != "__name__")
        .map(|(i, f)| {
            let expr = if i == value {
                computed.clone()
            } else if drop_metric_name && f.name == asap_types::ir::schema::PROMQL_SERIES_IDENTITY {
                ScalarExpr::FunctionCall {
                    name: "promql_drop_metric_name".into(),
                    args: vec![ScalarExpr::Column(i)],
                }
            } else {
                ScalarExpr::Column(i)
            };
            ProjectItem {
                alias: Some(f.name.clone()),
                expr,
            }
        })
        .collect();
    node(NonASAPOp::Project {
        child,
        cols,
        qualifier: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unresolved::UnresolvedPredicate;
    use asap_types::ir::operator::{
        BinaryOpKind, JoinKind, PromQLVectorSetOpKind, Source, VectorMatch,
    };
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::schema::{DataType, Field};
    use asap_types::ir::BinaryOperator;
    use asap_types::ir::ExprSemantics;
    use asap_types::types::AccuracyTarget;

    fn scan(metric: &str) -> UnresolvedOp {
        UnresolvedOp::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: None,
        }
    }

    fn named(n: &str) -> UnresolvedScalar {
        UnresolvedScalar::Column(ColumnRef::Named(n.into()))
    }

    fn eq_lit(col: UnresolvedScalar, v: &str) -> UnresolvedScalar {
        UnresolvedScalar::Compare {
            left: Box::new(col),
            op: CompareOpKind::Eq,
            right: Box::new(UnresolvedScalar::Literal(ScalarValue::Utf8(v.into()))),
            semantics: ExprSemantics::Promql,
        }
    }

    fn binary(kind: BinaryOpKind, vector_match: Option<VectorMatch>) -> BinaryOperator {
        BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind,
            vector_match,
        }
    }

    // Both sides resolve with qualifiers; an unknown right input is an error.
    #[test]
    fn resolve_pearson_corr_inputs() {
        let schema = Schema::new(vec![
            Field::plain("x", DataType::Float64, true).with_table("a"),
            Field::plain("x", DataType::Float64, true).with_table("b"),
        ]);
        let intent = AggIntent::PearsonCorr {
            left: ColumnRef::Qualified {
                table: "a".into(),
                name: "x".into(),
            },
            right: ColumnRef::Qualified {
                table: "b".into(),
                name: "x".into(),
            },
        };
        assert_eq!(
            resolve_agg_intent(&intent, &schema).unwrap(),
            AggIntent::PearsonCorr { left: 0, right: 1 }
        );
        let missing = AggIntent::PearsonCorr {
            left: ColumnRef::Qualified {
                table: "a".into(),
                name: "x".into(),
            },
            right: ColumnRef::Named("missing".into()),
        };
        assert!(resolve_agg_intent(&missing, &schema).is_err());
    }

    // Every leg resolves independently, qualifiers included; one unknown leg
    // fails rather than silently shortening the tuple.
    #[test]
    fn resolve_distinct_tuple_columns() {
        let schema = Schema::new(vec![
            Field::plain("k", DataType::Int64, true).with_table("a"),
            Field::plain("k", DataType::Int64, true).with_table("b"),
        ]);
        let qualified = |table: &str| ColumnRef::Qualified {
            table: table.into(),
            name: "k".into(),
        };
        let intent = AggIntent::Cardinality {
            cols: vec![qualified("b"), qualified("a")],
            accuracy: AccuracyTarget::Exact,
        };
        assert_eq!(
            resolve_agg_intent(&intent, &schema).unwrap(),
            AggIntent::Cardinality {
                cols: vec![1, 0],
                accuracy: AccuracyTarget::Exact,
            }
        );
        let missing = AggIntent::Cardinality {
            cols: vec![qualified("a"), ColumnRef::Named("missing".into())],
            accuracy: AccuracyTarget::Exact,
        };
        assert!(resolve_agg_intent(&missing, &schema).is_err());
    }

    // `<vector> > <scalar>`: the bridged literal comes through unchanged, the
    // vector side binds positionally, the `VectorMatch` survives untouched, and
    // the node's schema follows the vector side.
    #[test]
    fn scalar_comparison_preserves_vector_values_and_labels() {
        let unresolved = UnresolvedOp::PromqlScalarOp {
            child: Rc::new(scan("up")),
            scalar: UnresolvedScalar::Literal(ScalarValue::Float64(1.0)),
            op: BinaryOpKind::Compare(CompareOpKind::Gt),
            scalar_left: true,
            return_bool: false,
        };
        let resolved = resolve_root(&unresolved).unwrap();
        let NonASAPOp::Filter {
            child,
            pred: Predicate(ScalarExpr::Compare { left, right, .. }),
        } = resolved.expect_non_asap()
        else {
            panic!("expected Filter")
        };
        assert_eq!(**left, ScalarExpr::literal_f64(1.0));
        assert_eq!(
            **right,
            ScalarExpr::Column(child.schema.column_id("value").unwrap())
        );
        assert_eq!(resolved.schema, child.schema);
        assert!(resolved.schema.has_promql_series_identity());
    }

    // A `Concat` discriminator column referenced nowhere else, over a
    // schemaless first branch, resolves to the branch's own positional ids.
    #[test]
    fn resolve_root_seeds_and_resolves_an_otherwise_unreferenced_discriminator_column() {
        let unresolved = UnresolvedOp::concat_with_discriminator(
            vec![scan("m"), scan("m")],
            ColumnRef::Named("phi".into()),
            vec![ColumnRef::Named("host".into())],
        );

        let resolved = resolve_root(&unresolved).expect("resolves");
        let NonASAPOp::Concat {
            children,
            discriminator_unique_key,
        } = resolved.expect_non_asap()
        else {
            panic!("expected a resolved Concat, got {resolved:?}");
        };
        let schema = &children[0].schema;
        let key = discriminator_unique_key
            .as_ref()
            .expect("discriminator key survives resolution");
        assert_eq!(*key.discriminator(), schema.column_id("phi").unwrap());
        assert_eq!(
            key.inner_key().to_vec(),
            vec![schema.column_id("host").unwrap()]
        );
    }

    // `sum by (job)(a or b)`: each `BinaryOp` side binds in its own scope but
    // inherits the enclosing aggregate's group key (issue #52).
    #[test]
    fn binary_op_sides_inherit_enclosing_group_keys() {
        let unresolved = UnresolvedOp::Aggregate {
            reduction: Reduction::by(vec![ColumnRef::Named("job".into())]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::new(UnresolvedOp::BinaryOp {
                operator: binary(BinaryOpKind::Set(PromQLVectorSetOpKind::Or), None),
                return_bool: false,
                lhs: Rc::new(scan("a")),
                rhs: Rc::new(scan("b")),
            }),
        };
        let resolved = resolve_root(&unresolved).expect("resolves");
        let NonASAPOp::Aggregate {
            reduction, child, ..
        } = resolved.expect_non_asap()
        else {
            panic!("expected Aggregate");
        };
        let NonASAPOp::BinaryOp { lhs, rhs, .. } = child.expect_non_asap() else {
            panic!("expected BinaryOp");
        };
        let job = lhs.schema.column_id("job").expect("lhs sees job");
        assert_eq!(rhs.schema.column_id("job"), Some(job));
        assert_eq!(reduction.expect_reduce().keys(), &[job]);
        assert_eq!(resolved.schema.fields[0].name, "job");
    }

    // HAVING binds against the aggregate's output, not its input.
    #[test]
    fn having_resolves_against_aggregate_output() {
        let input = Schema::new(vec![
            Field::plain("k", DataType::Utf8, false),
            Field::plain("v", DataType::Float64, false),
        ]);
        let unresolved = UnresolvedOp::Aggregate {
            reduction: Reduction::by(vec![ColumnRef::Named("k".into())]),
            measures: vec![AggIntent::Sum {
                col: Some(ColumnRef::Named("v".into())),
            }],
            output_names: vec!["total".into()],
            filters: vec![],
            having: Some(UnresolvedPredicate(UnresolvedScalar::Compare {
                left: Box::new(named("total")),
                op: CompareOpKind::Gt,
                right: Box::new(UnresolvedScalar::Literal(ScalarValue::Float64(1.0))),
                semantics: ExprSemantics::Sql,
            })),
            child: Rc::new(UnresolvedOp::Scan {
                source: Source::Table {
                    table_ref: "t".into(),
                },
                predicates: vec![],
                schema: Some(input),
            }),
        };
        let resolved = resolve_root(&unresolved).expect("resolves");
        let NonASAPOp::Aggregate {
            having: Some(Predicate(ScalarExpr::Compare { left, .. })),
            ..
        } = resolved.expect_non_asap()
        else {
            panic!("expected Aggregate with HAVING");
        };
        assert_eq!(**left, ScalarExpr::Column(1));
        assert_eq!(resolved.schema.fields[1].name, "total");
    }

    // A join predicate binds against left ++ right; a qualified reference
    // picks the right side even when both inputs share the column name.
    #[test]
    fn join_predicate_resolves_against_left_then_right() {
        let side = |table: &str| UnresolvedOp::Scan {
            source: Source::Table {
                table_ref: table.into(),
            },
            predicates: vec![],
            schema: Some(Schema::new(vec![
                Field::plain("k", DataType::Int64, false).with_table(table)
            ])),
        };
        let qualified = |table: &str| {
            UnresolvedScalar::Column(ColumnRef::Qualified {
                table: table.into(),
                name: "k".into(),
            })
        };
        let unresolved = UnresolvedOp::Join {
            kind: JoinKind::Inner,
            pred: UnresolvedPredicate(UnresolvedScalar::Compare {
                left: Box::new(qualified("b")),
                op: CompareOpKind::Eq,
                right: Box::new(qualified("a")),
                semantics: ExprSemantics::Sql,
            }),
            left: Rc::new(side("a")),
            right: Rc::new(side("b")),
        };
        let resolved = resolve_root(&unresolved).expect("resolves");
        let NonASAPOp::Join {
            pred: Predicate(ScalarExpr::Compare { left, right, .. }),
            ..
        } = resolved.expect_non_asap()
        else {
            panic!("expected Join");
        };
        assert_eq!(**left, ScalarExpr::Column(1));
        assert_eq!(**right, ScalarExpr::Column(0));
    }

    // `m * scalar(x{a="1"})`: the operator inside the scalar operand is bound
    // as a root in its own scope — its matcher label seeds its own leaf, not
    // the vector side's.
    #[test]
    fn scalar_from_vector_operand_binds_in_its_own_scope() {
        let x = UnresolvedOp::Scan {
            source: Source::TimeSeries { metric: "x".into() },
            predicates: vec![UnresolvedPredicate(eq_lit(named("a"), "1"))],
            schema: None,
        };
        let unresolved = UnresolvedOp::PromqlScalarOp {
            child: Rc::new(scan("m")),
            scalar: UnresolvedScalar::PromqlScalarFromVector(Rc::new(x)),
            op: BinaryOpKind::Arithmetic(asap_types::ir::scalar::ArithmeticOpKind::Mul),
            scalar_left: false,
            return_bool: false,
        };
        let resolved = resolve_root(&unresolved).unwrap();
        let NonASAPOp::Project {
            child: lhs, cols, ..
        } = resolved.expect_non_asap()
        else {
            panic!("expected Project")
        };
        assert!(lhs.schema.column_id("a").is_none());
        let ScalarExpr::Arithmetic { right, .. } = &cols[1].expr else {
            panic!("expected arithmetic")
        };
        let ScalarExpr::PromqlScalarFromVector(inner) = right.as_ref() else {
            panic!("expected scalar(v)")
        };
        let a = inner
            .schema
            .column_id("a")
            .expect("own matcher label seeded");
        let NonASAPOp::Scan { predicates, .. } = inner.expect_non_asap() else {
            panic!("expected Scan");
        };
        let Predicate(ScalarExpr::Compare { left, .. }) = &predicates[0] else {
            panic!("expected Compare");
        };
        assert_eq!(**left, ScalarExpr::Column(a));
        assert_eq!(resolved.schema.fields.len(), lhs.schema.fields.len());
    }

    // PromQL grouping drops a key provably absent from a closed input (#53):
    // `sum(sum by (group)(m)) by (job)`.
    #[test]
    fn nested_aggregate_drops_absent_promql_group_key() {
        let inner = UnresolvedOp::Aggregate {
            reduction: Reduction::by(vec![ColumnRef::Named("group".into())]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::new(scan("m")),
        };
        let outer = UnresolvedOp::Aggregate {
            reduction: Reduction::by(vec![ColumnRef::Named("job".into())]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::new(inner),
        };
        let resolved = resolve_root(&outer).expect("resolves");
        let NonASAPOp::Aggregate { reduction, .. } = resolved.expect_non_asap() else {
            panic!("expected Aggregate");
        };
        assert!(reduction.expect_reduce().keys().is_empty());
    }
}
