//! Layer-2 → canonical L3 IR converter.
//!
//! Recursively converts a whole [`relational::QueryExpr`] tree into a whole
//! [`query_expr::QueryExpr`] tree. The single-statistic sketchable `Aggregate`
//! fuses directly in canonical terms (window-swap, `Partition` wrap); see the
//! `Aggregate` arm.
//!
//! Name resolution is an explicit pass: [`convert_root`] runs the
//! [`Binder`](super::binder) first to build the complete, self-contained
//! schema every `ColumnId` indexes into, so positional resolution downstream
//! is total.

use thiserror::Error;

use crate::intent_algebra::agg_intent::AggIntent;
use crate::intent_algebra::binder::Binder;
use crate::intent_algebra::column_resolution::{
    output_schema_for_aggregate, resolve_column_refs, resolve_expr, resolve_named_keys,
    ResolveError,
};
use crate::intent_algebra::expr_ir::{ColumnRef, L2Expr, L3Expr, L3Scalar};
use crate::intent_algebra::names::BindingName;
use crate::intent_algebra::query_expr::{
    PartitionKeys as CPartitionKeys, Predicate, ProjectItem, QueryExpr as CQueryExpr, SortKey,
    Source, WindowKind,
};
use crate::intent_algebra::relational::{AggFunc, QueryExpr as LQueryExpr, SourceSpec};
use crate::intent_algebra::schema::{ColumnId, Schema};
use crate::types::AccuracyTarget;

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
    Schema(#[from] crate::intent_algebra::query_expr::QueryExprError),
}

/// Lower a Layer-2 tree to canonical L3, threading `accuracy` onto every
/// approximate intent (`Count`, `Quantile`, `Cardinality`, `TopK`).
pub fn convert_root(
    legacy: &LQueryExpr,
    accuracy: &AccuracyTarget,
) -> Result<CQueryExpr, ConvertError> {
    let fallback = Binder::new().bind(legacy);
    convert(legacy, &fallback, accuracy)
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
            aggs,
            having,
            input,
        } => {
            // Single-statistic aggregate (no HAVING) over a *time-series* leaf
            // fuses: a `Window` input becomes `Window { Aggregate { by: [] } }`;
            // GROUP BY keys wrap the result in a `Partition` (the streaming
            // sketch canonical shape). Tabular (SQL) GROUP BY instead falls
            // through to the positional `Aggregate.by` path below, so the group
            // keys land in the output schema (a SELECT projects them). The
            // reducer's input column resolves against the aggregate's *direct*
            // input (the scan under any window).
            if aggs.len() == 1 && having.is_none() && !input.leaf_is_tabular() {
                let (agg_input_l2, window): (&LQueryExpr, Option<(_, _)>) = match input.as_ref() {
                    LQueryExpr::Window {
                        duration,
                        slide,
                        input: win_input,
                    } => (win_input, Some((*duration, *slide))),
                    other => (other, None),
                };
                let agg_child = convert(agg_input_l2, fallback, acc)?;
                let agg_in_schema = agg_child.output_schema()?;
                let intent = agg_func_to_intent(
                    &aggs[0].func,
                    acc,
                    resolve_agg_col(&aggs[0].col, &agg_in_schema)?,
                );
                let aggregate = CQueryExpr::Aggregate {
                    by: Vec::new(),
                    aggs: vec![intent],
                    output_names: vec![aggs[0].alias.clone()],
                    having: None,
                    child: Box::new(agg_child),
                };
                let sketch = match window {
                    Some((duration, slide)) => CQueryExpr::Window {
                        kind: if slide.is_some() {
                            WindowKind::Sliding
                        } else {
                            WindowKind::Tumbling
                        },
                        size: duration,
                        slide,
                        child: Box::new(aggregate),
                    },
                    None => aggregate,
                };
                return Ok(if keys.is_empty() {
                    sketch
                } else {
                    CQueryExpr::Partition {
                        keys: CPartitionKeys::By(keys.clone()),
                        child: Box::new(sketch),
                    }
                });
            }

            // Plain canonical `Aggregate`: multi-agg or HAVING-bearing. Keys +
            // per-reducer input columns resolve against the child's (input)
            // schema; HAVING references the aggregate's *output* columns, so it
            // resolves against the derived output schema instead.
            let child = convert(input, fallback, acc)?;
            let child_schema = child.output_schema()?;
            let by = resolve_named_keys(keys, &child_schema)?;
            let intents: Vec<AggIntent> = aggs
                .iter()
                .map(|item| -> Result<AggIntent, ConvertError> {
                    let col = resolve_agg_col(&item.col, &child_schema)?;
                    Ok(agg_func_to_intent(&item.func, acc, col))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let output_names: Vec<String> = aggs.iter().map(|item| item.alias.clone()).collect();
            let having = having
                .as_ref()
                .map(|h| -> Result<Predicate, ConvertError> {
                    let out_schema =
                        output_schema_for_aggregate(&child_schema, &by, &intents, &output_names);
                    Ok(Predicate(resolve_expr(h, &out_schema)?))
                })
                .transpose()?;
            CQueryExpr::Aggregate {
                by,
                aggs: intents,
                output_names,
                having,
                child: Box::new(child),
            }
        }

        LQueryExpr::Window {
            duration,
            slide,
            input,
        } => CQueryExpr::Window {
            kind: if slide.is_some() {
                WindowKind::Sliding
            } else {
                WindowKind::Tumbling
            },
            size: *duration,
            slide: *slide,
            child: Box::new(convert(input, fallback, acc)?),
        },

        // π — resolve each project item's expression to positional against the
        // child's schema.
        LQueryExpr::Project { cols, input } => {
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
                child: Box::new(child),
            }
        }

        LQueryExpr::Partition { keys, input } => CQueryExpr::Partition {
            keys: keys.clone(),
            child: Box::new(convert(input, fallback, acc)?),
        },

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
            let by = resolve_named_keys(by, &child_schema)?;
            CQueryExpr::Aggregate {
                by,
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

        LQueryExpr::Sort { keys, input } => {
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
            CQueryExpr::Sort {
                keys,
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
            let partition_by = resolve_named_keys(partition_by, &child_schema)?;
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
        } => CQueryExpr::BinaryOp {
            op: op.clone(),
            // A binary op's two sides may scan different metrics with different
            // label sets, so each branch must resolve against its OWN bound
            // schema. `convert_root` re-runs the Binder per sub-tree; threading
            // the parent `schema` (derived from the left leaf only) would bind
            // the right side's columns to the wrong positions.
            lhs: Box::new(convert_root(lhs, acc)?),
            rhs: Box::new(convert_root(rhs, acc)?),
            vector_match: vector_match.clone(),
        },
    })
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
    Ok(CQueryExpr::Scan {
        source,
        predicates,
        schema,
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
            q: *q,
            accuracy: acc.clone(),
        },
        AggFunc::CountDistinct => AggIntent::Cardinality {
            accuracy: acc.clone(),
        },
        AggFunc::HeavyHitters { k } => AggIntent::TopK {
            k: *k as usize,
            accuracy: acc.clone(),
        },
        AggFunc::Rate { window } => AggIntent::Rate { window: *window },
        AggFunc::Increase { window } => AggIntent::Increase { window: *window },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_algebra::agg_intent::AggIntent;
    use crate::intent_algebra::expr_ir::{CompareOp, L2Expr, L3Expr, L3Scalar};
    use crate::intent_algebra::query_expr::{JoinKind, QueryExpr as CQueryExpr};
    use crate::intent_algebra::relational::{
        AggFunc, AggItem, QueryExpr as LQueryExpr, SourceSpec,
    };
    use crate::intent_algebra::schema::{Column, DataType, Schema};

    fn col(name: &str, dtype: DataType) -> Column {
        Column::new(name, dtype, false)
    }

    /// A SQL-shaped `SELECT SUM(bytes), AVG(latency) FROM t` lowers each
    /// reducer onto its own input column (positional), and the derived output
    /// schema types each result off that column (`SUM(bytes:Int64)→Int64`).
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
            aggs: vec![
                AggItem {
                    alias: "total_bytes".into(),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                    distinct: false,
                },
                AggItem {
                    alias: "avg_latency".into(),
                    func: AggFunc::Avg,
                    col: ColumnRef::Named("latency".into()),
                    distinct: false,
                },
            ],
            having: None,
            input: Box::new(LQueryExpr::Source(SourceSpec::new("t"))),
        };

        let l3 = convert(&tree, &schema, &AccuracyTarget::Exact).unwrap();
        let CQueryExpr::Aggregate { by, aggs, .. } = &l3 else {
            panic!("expected Aggregate, got {l3:?}");
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
            aggs: vec![AggItem {
                alias: "value".into(),
                func: AggFunc::Sum,
                col: ColumnRef::SampleValue,
                distinct: false,
            }],
            having: None,
            input: Box::new(LQueryExpr::Source(SourceSpec::new("m"))),
        };
        let l3 = convert(&tree, &schema, &AccuracyTarget::Exact).unwrap();
        // single-agg fused path → bare Aggregate (no keys → no Partition)
        let CQueryExpr::Aggregate { aggs, .. } = &l3 else {
            panic!("expected Aggregate, got {l3:?}");
        };
        assert_eq!(aggs, &vec![AggIntent::Sum { col: None }]);
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
            keys: vec!["region".into()],
            aggs: vec![
                AggItem {
                    alias: "tot".into(),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                    distinct: false,
                },
                AggItem {
                    alias: "n".into(),
                    func: AggFunc::Count,
                    col: ColumnRef::Wildcard,
                    distinct: false,
                },
            ],
            having: None,
            input: Box::new(join),
        };
        let l3 = convert_root(&tree, &AccuracyTarget::Exact).unwrap();
        let CQueryExpr::Aggregate {
            by, aggs, child, ..
        } = &l3
        else {
            panic!("expected multi-agg Aggregate, got {l3:?}");
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
            keys: vec!["region".into()],
            aggs: vec![
                AggItem {
                    alias: "tot".into(),
                    func: AggFunc::Sum,
                    col: ColumnRef::Named("bytes".into()),
                    distinct: false,
                },
                AggItem {
                    alias: "n".into(),
                    func: AggFunc::Count,
                    col: ColumnRef::Wildcard,
                    distinct: false,
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
