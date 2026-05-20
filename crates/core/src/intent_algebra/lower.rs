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
    PartitionKeys as CPartitionKeys, Predicate, QueryExpr as CQueryExpr, Source, WindowKind,
};
use crate::intent_algebra::relational::{AggFunc, QueryExpr as LQueryExpr};
use crate::intent_algebra::schema::Schema;
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
                let intent = agg_func_to_intent(&aggs[0].func, acc);
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
                .map(|item| agg_func_to_intent(&item.func, acc))
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
            left: Box::new(convert(left, schema, acc)?),
            right: Box::new(convert(right, schema, acc)?),
        },

        LQueryExpr::SetOp {
            kind,
            all,
            left,
            right,
        } => CQueryExpr::SetOp {
            kind: kind.clone(),
            all: *all,
            left: Box::new(convert(left, schema, acc)?),
            right: Box::new(convert(right, schema, acc)?),
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
            lhs: Box::new(convert(lhs, schema, acc)?),
            rhs: Box::new(convert(rhs, schema, acc)?),
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

/// Map a Layer-2 [`AggFunc`] to its canonical [`AggIntent`], threading the
/// workload's accuracy target onto the approximate intents.
fn agg_func_to_intent(func: &AggFunc, acc: &AccuracyTarget) -> AggIntent {
    match func {
        AggFunc::Count => AggIntent::Count {
            accuracy: acc.clone(),
        },
        AggFunc::Sum => AggIntent::Sum,
        AggFunc::Avg => AggIntent::Avg,
        AggFunc::Min => AggIntent::Min,
        AggFunc::Max => AggIntent::Max,
        AggFunc::StdDev { population } => AggIntent::StdDev {
            population: *population,
        },
        AggFunc::Variance { population } => AggIntent::Variance {
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
