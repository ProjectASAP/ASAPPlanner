//! Workload-level Common Sub-Expression Elimination.
//!
//! Multi-root planning hoists shared sub-DAGs into `LetBinding`s so the cost
//! model can credit the producer once. Legality is gated by
//! [`cse_reuse_is_legal`](super::schema::cse_reuse_is_legal): a candidate
//! sub-DAG only becomes a binding when its output schema has at least one
//! `unique_keys` set — the load-bearing field for this pass.
//!
//! Scope: the basic "≥2 roots with identical `Aggregate`-child sub-trees"
//! case. The fully-general algorithm (alpha-equivalence, schema-merge,
//! nested CSE) is a downstream optimisation, not part of the IR contract.

use crate::intent_algebra::names::{BindingName, QueryId};
use crate::intent_algebra::query_expr::QueryExpr;
use crate::intent_algebra::schema::cse_reuse_is_legal;

/// Multi-root container produced by the CSE pass.
#[derive(Debug, Clone, PartialEq)]
pub struct CseWorkloadPlan {
    /// Named shared producers, each referenced by ≥2 roots via `QueryExpr::Ref`.
    pub bindings: Vec<(BindingName, QueryExpr)>,
    /// One root per input query, in input order.
    pub roots: Vec<(QueryId, QueryExpr)>,
}

/// Hoist sub-expressions structurally identical across ≥2 roots into shared
/// `LetBinding`s, leaving each root with a `Ref` where the duplicate lived.
///
/// A candidate is hoisted only when
/// `cse_reuse_is_legal(&candidate.output_schema(), consumers)` returns `Ok`.
pub fn dedupe_subtrees(roots: Vec<(QueryId, QueryExpr)>) -> CseWorkloadPlan {
    if roots.len() < 2 {
        return CseWorkloadPlan {
            bindings: vec![],
            roots,
        };
    }

    // Count `Aggregate`-child sub-trees that appear in ≥2 roots. Group by
    // structural equality (`QueryExpr: PartialEq`) rather than `Debug`
    // output: `{:?}` is not a guaranteed-injective, stable identity contract.
    // The candidate set is one entry per distinct root child, so this linear
    // scan is bounded by the number of distinct queries.
    let mut candidate_counts: Vec<(QueryExpr, usize)> = Vec::new();
    for (_, root) in &roots {
        if let QueryExpr::Aggregate { child, .. } = root {
            if matches!(**child, QueryExpr::Ref { .. }) {
                continue;
            }
            match candidate_counts
                .iter_mut()
                .find(|(e, _)| e == child.as_ref())
            {
                Some(entry) => entry.1 += 1,
                None => candidate_counts.push(((**child).clone(), 1)),
            }
        }
    }

    // Pick the most-shared legal candidate (biggest reuse first).
    let mut chosen: Option<(QueryExpr, usize)> = None;
    for (expr, count) in candidate_counts.into_iter() {
        if count < 2 {
            continue;
        }
        let Ok(out_schema) = expr.output_schema() else {
            continue;
        };
        if cse_reuse_is_legal(&out_schema, count).is_err() {
            continue;
        }
        match &chosen {
            Some((_, best)) if *best >= count => {}
            _ => chosen = Some((expr, count)),
        }
    }

    let Some((shared_expr, _)) = chosen else {
        return CseWorkloadPlan {
            bindings: vec![],
            roots,
        };
    };

    let binding_name = BindingName::new("shared_0");
    let mut rewritten: Vec<(QueryId, QueryExpr)> = Vec::with_capacity(roots.len());
    for (qid, root) in roots {
        let new_root = match root {
            QueryExpr::Aggregate {
                by,
                aggs,
                having,
                child,
            } if *child == shared_expr => QueryExpr::Aggregate {
                by,
                aggs,
                having,
                child: Box::new(QueryExpr::Ref {
                    name: binding_name.clone(),
                }),
            },
            other => other,
        };
        rewritten.push((qid, new_root));
    }

    CseWorkloadPlan {
        bindings: vec![(binding_name, shared_expr)],
        roots: rewritten,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_algebra::agg_intent::AggIntent;
    use crate::intent_algebra::query_expr::{Source, WindowKind};
    use crate::intent_algebra::schema::{Column, DataType, Schema};
    use crate::types::AccuracyTarget;
    use std::time::Duration;

    fn col(name: &str, dtype: DataType) -> Column {
        Column {
            name: name.into(),
            dtype,
            nullable: false,
        }
    }

    fn ts_scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: "http_request_duration_seconds".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp),
                    col("service", DataType::Utf8),
                    col("value", DataType::Float64),
                ],
                0,
                vec![vec![0, 1]],
            ),
        }
    }

    fn windowed_scan() -> QueryExpr {
        QueryExpr::Window {
            kind: WindowKind::Sliding,
            size: Duration::from_secs(300),
            slide: None,
            child: Box::new(ts_scan()),
        }
    }

    #[test]
    fn dedupe_subtrees_single_root_passthrough() {
        let q = QueryExpr::Aggregate {
            by: vec![1],
            aggs: vec![AggIntent::Quantile {
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.01),
            }],
            having: None,
            child: Box::new(windowed_scan()),
        };
        let out = dedupe_subtrees(vec![(QueryId::new("q1"), q.clone())]);
        assert!(out.bindings.is_empty());
        assert_eq!(out.roots[0].1, q);
    }

    #[test]
    fn dedupe_subtrees_basic() {
        let mk = |q: f64| QueryExpr::Aggregate {
            by: vec![1],
            aggs: vec![AggIntent::Quantile {
                q,
                accuracy: AccuracyTarget::Epsilon(0.01),
            }],
            having: None,
            child: Box::new(windowed_scan()),
        };
        let out = dedupe_subtrees(vec![
            (QueryId::new("q1"), mk(0.99)),
            (QueryId::new("q2"), mk(0.95)),
        ]);
        assert_eq!(out.bindings.len(), 1);
        assert_eq!(out.bindings[0].0, BindingName::new("shared_0"));
        assert_eq!(out.bindings[0].1, windowed_scan());
        for (_, root) in &out.roots {
            match root {
                QueryExpr::Aggregate { child, .. } => assert_eq!(
                    **child,
                    QueryExpr::Ref {
                        name: BindingName::new("shared_0")
                    }
                ),
                other => panic!("expected Aggregate root, got {other:?}"),
            }
        }
    }

    #[test]
    fn dedupe_subtrees_no_shared_subexpr_when_unique_keys_absent() {
        // Schema without unique_keys → CSE refuses to share even if identical.
        let scan_no_uk = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp),
                    col("value", DataType::Float64),
                ],
                0,
                vec![],
            ),
        };
        let mk = || QueryExpr::Aggregate {
            by: vec![],
            aggs: vec![AggIntent::Sum],
            having: None,
            child: Box::new(scan_no_uk.clone()),
        };
        let out = dedupe_subtrees(vec![(QueryId::new("q1"), mk()), (QueryId::new("q2"), mk())]);
        assert!(out.bindings.is_empty(), "no unique_keys → no hoisting");
    }
}
