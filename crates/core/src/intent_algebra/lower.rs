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
use crate::intent_algebra::column_resolution::{resolve_named_keys, ResolveError};
use crate::intent_algebra::names::BindingName;
use crate::intent_algebra::query_expr::{
    ColumnRef, PartitionKeys as CPartitionKeys, Predicate, QueryExpr as CQueryExpr, Source,
    WindowKind,
};
use crate::intent_algebra::relational::{AggFunc, QueryExpr as LQueryExpr};
use crate::intent_algebra::schema::{ColumnId, Schema};
use crate::types::AccuracyTarget;

/// Errors produced while converting a Layer-2 tree to canonical.
#[derive(Debug, Error)]
pub enum ConvertError {
    /// A column reference (`Aggregate` key, `Partition` / `TopK` key) did not
    /// resolve against the inherited schema.
    #[error("column resolution failed: {0}")]
    Resolve(#[from] ResolveError),
}

/// Lower a Layer-2 tree to canonical L3, threading `accuracy` onto every
/// approximate intent (`Count`, `Quantile`, `Cardinality`, `TopK`).
pub fn convert_root(
    legacy: &LQueryExpr,
    accuracy: &AccuracyTarget,
) -> Result<CQueryExpr, ConvertError> {
    let schema = Binder::new().bind(legacy);
    convert(legacy, &schema, accuracy)
}

/// Convert a Layer-2 tree to canonical against an explicit inherited schema.
pub fn convert(
    legacy: &LQueryExpr,
    schema: &Schema,
    acc: &AccuracyTarget,
) -> Result<CQueryExpr, ConvertError> {
    Ok(match legacy {
        LQueryExpr::Source(spec) => scan(spec.name.clone(), schema, Vec::new()),

        LQueryExpr::Ref(name) => CQueryExpr::Ref {
            name: BindingName::new(name.clone()),
        },

        // Fold label matchers / pushed-down predicates directly onto the Scan
        // when the immediate child is a `Source`; otherwise emit a `Filter`.
        LQueryExpr::Filter { pred, input } => match input.as_ref() {
            LQueryExpr::Source(spec) => {
                let predicates = pred.conjuncts().iter().cloned().map(Predicate).collect();
                scan(spec.name.clone(), schema, predicates)
            }
            other => CQueryExpr::Filter {
                pred: Predicate(pred.clone()),
                child: Box::new(convert(other, schema, acc)?),
            },
        },

        LQueryExpr::Aggregate {
            keys,
            aggs,
            having,
            input,
        } => {
            // Single-statistic aggregate (no HAVING) fuses: a `Window` input
            // becomes `Window { Aggregate { by: [] } }`; GROUP BY keys wrap the
            // result in a `Partition`.
            if aggs.len() == 1 && having.is_none() {
                let intent =
                    agg_func_to_intent(&aggs[0].func, acc, resolve_agg_col(&aggs[0].col, schema));
                let sketch = match input.as_ref() {
                    LQueryExpr::Window {
                        duration,
                        slide,
                        input: win_input,
                    } => CQueryExpr::Window {
                        kind: if slide.is_some() {
                            WindowKind::Sliding
                        } else {
                            WindowKind::Tumbling
                        },
                        size: *duration,
                        slide: *slide,
                        child: Box::new(CQueryExpr::Aggregate {
                            by: Vec::new(),
                            aggs: vec![intent],
                            having: None,
                            child: Box::new(convert(win_input, schema, acc)?),
                        }),
                    },
                    other => CQueryExpr::Aggregate {
                        by: Vec::new(),
                        aggs: vec![intent],
                        having: None,
                        child: Box::new(convert(other, schema, acc)?),
                    },
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

            // Plain canonical `Aggregate`: multi-agg or HAVING-bearing.
            let by = resolve_named_keys(keys, schema)?;
            let intents = aggs
                .iter()
                .map(|item| agg_func_to_intent(&item.func, acc, resolve_agg_col(&item.col, schema)))
                .collect();
            CQueryExpr::Aggregate {
                by,
                aggs: intents,
                having: having.clone().map(Predicate),
                child: Box::new(convert(input, schema, acc)?),
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
            child: Box::new(convert(input, schema, acc)?),
        },

        LQueryExpr::Partition { keys, input } => CQueryExpr::Partition {
            keys: keys.clone(),
            child: Box::new(convert(input, schema, acc)?),
        },

        LQueryExpr::Distinct { cols, input } => CQueryExpr::Distinct {
            cols: cols.clone(),
            child: Box::new(convert(input, schema, acc)?),
        },

        LQueryExpr::TopK { k, by, input } => {
            let by = resolve_named_keys(by, schema)?;
            CQueryExpr::Aggregate {
                by,
                aggs: vec![AggIntent::TopK {
                    k: *k as usize,
                    accuracy: acc.clone(),
                }],
                having: None,
                child: Box::new(convert(input, schema, acc)?),
            }
        }

        LQueryExpr::Merge { inputs } => CQueryExpr::Merge {
            children: inputs
                .iter()
                .map(|i| convert(i, schema, acc))
                .collect::<Result<Vec<_>, _>>()?,
        },

        LQueryExpr::Join {
            kind,
            pred,
            left,
            right,
        } => CQueryExpr::Join {
            kind: kind.clone(),
            pred: Predicate(pred.clone().unwrap_or(
                crate::intent_algebra::expr_ir::L3Expr::Literal(
                    crate::intent_algebra::expr_ir::L3Scalar::Boolean(true),
                ),
            )),
            // Each branch is bound independently — see the `BinaryOp` arm.
            left: Box::new(convert_root(left, acc)?),
            right: Box::new(convert_root(right, acc)?),
        },

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

        LQueryExpr::Sort { keys, input } => CQueryExpr::Sort {
            keys: keys.clone(),
            child: Box::new(convert(input, schema, acc)?),
        },

        LQueryExpr::Limit { n, offset, input } => CQueryExpr::Limit {
            n: *n as usize,
            offset: *offset as usize,
            child: Box::new(convert(input, schema, acc)?),
        },

        LQueryExpr::LetBinding { name, expr, body } => CQueryExpr::LetBinding {
            name: BindingName::new(name.clone()),
            expr: Box::new(convert(expr, schema, acc)?),
            child: Box::new(convert(body, schema, acc)?),
        },

        LQueryExpr::PromQLSubquery {
            range,
            resolution,
            input,
        } => CQueryExpr::Subquery {
            range: *range,
            resolution: *resolution,
            child: Box::new(convert(input, schema, acc)?),
        },

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

/// Build a canonical `Scan` over a time-series source carrying the Binder's
/// self-contained schema.
fn scan(metric: String, schema: &Schema, predicates: Vec<Predicate>) -> CQueryExpr {
    CQueryExpr::Scan {
        source: Source::TimeSeries { metric },
        predicates,
        schema: schema.clone(),
    }
}

/// Resolve a Layer-2 aggregate-input [`ColumnRef`] to a positional input
/// column. `SampleValue` / `Wildcard` carry no specific column → `None` (the
/// PromQL sample-value convention); a named column (`SUM(bytes)`) resolves to
/// its position so the L3 reducer types off the right input.
fn resolve_agg_col(col: &ColumnRef, schema: &Schema) -> Option<ColumnId> {
    match col {
        ColumnRef::Named(name) => schema.column_id(name),
        ColumnRef::SampleValue | ColumnRef::Wildcard => None,
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
    use crate::intent_algebra::query_expr::QueryExpr as CQueryExpr;
    use crate::intent_algebra::relational::{
        AggFunc, AggItem, QueryExpr as LQueryExpr, SourceSpec,
    };
    use crate::intent_algebra::schema::{Column, DataType, Schema};

    fn col(name: &str, dtype: DataType) -> Column {
        Column {
            name: name.into(),
            dtype,
            nullable: false,
        }
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
            input: Box::new(LQueryExpr::Source(SourceSpec { name: "t".into() })),
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

        // Output schema types each reducer off its own input column.
        let out = l3.output_schema().unwrap();
        assert_eq!(out.columns[0], col("sum", DataType::Int64)); // SUM(bytes:Int64)
        assert_eq!(out.columns[1], col("avg", DataType::Float64)); // AVG(latency)→Float64
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
            input: Box::new(LQueryExpr::Source(SourceSpec { name: "m".into() })),
        };
        let l3 = convert(&tree, &schema, &AccuracyTarget::Exact).unwrap();
        // single-agg fused path → bare Aggregate (no keys → no Partition)
        let CQueryExpr::Aggregate { aggs, .. } = &l3 else {
            panic!("expected Aggregate, got {l3:?}");
        };
        assert_eq!(aggs, &vec![AggIntent::Sum { col: None }]);
    }
}
