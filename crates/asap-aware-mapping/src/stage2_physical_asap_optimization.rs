//! #509 Stage 2, physical ASAP-aware optimization (MVP): turns one
//! CandidateLogicalASAPDAG into CandidatePhysicalASAPDAGs by materialization
//! and physical operator implementation. Materialization offers one option
//! for now: every sub-DAG is `Materialization::NotMaterialized` and runs at
//! query time, so each logical candidate yields exactly one physical
//! candidate. Enumerating
//! `MatAtIngestion(medium)` and `MatAtQuery(medium)` per sub-DAG
//! (Algorithm 3) comes in #604. A candidate this stage cannot implement
//! fails with a [`PhysicalASAPOptimizationError`], and the caller prunes that candidate alone,
//! with the error as its reason.
//!
//! The runtime has one implementation per logical operator except exact
//! top-k, which it cannot run as `Aggregate{[TopK]}`. Stage 2 records that
//! choice in the DAG by rewriting it to a per-group sort followed by a
//! per-group limit, the shape the PromQL frontend uses for generic `topk`.
//! A `Project` over the limited rows restores the logical `[keys.., topk_k]`
//! columns, so an exact root has the same column names as a summary
//! candidate's and as the frontend declares, and no input column leaks.
//! `topk_k` carries the ranked value's type and nullability (the frontend
//! declares a non-null `Utf8` placeholder); casting the value to `Utf8` would
//! not make it the same answer.
//! An `Aggregate{[TopK]}` over a per-entity reduction, or with other
//! measures, a filter or `HAVING`, has no such rewrite, and its candidate
//! fails.
//! A summary needs no rewrite: `SummaryAgg` → `SummaryEstimate` already is
//! build → estimate. Every node keeps the guarantee Stage 1 composed for it.
use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG,
};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError, SortKey, TimingMemo,
};
use asap_types::post_asap::{ExecutionDataStateError, Materialization};
use asap_types::pre_asap::column_resolution::resolve_column_ref;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::schema::ColumnId;
use asap_types::pre_asap::{AggIntent, Reduction};
use thiserror::Error;

/// One CandidatePhysicalASAPDAG (Stage 2 output), derived from exactly one
/// CandidateLogicalASAPDAG (Stage 1 output).
/// `roots` are the timed query roots (one per query, in workload order)
/// that `dag` exports.
#[derive(Debug, Clone)]
pub struct PhysicalCandidate {
    pub id: String,
    pub from_logical: String,
    /// Human-readable name of the candidate, used only for display (see
    /// `pass::stage1_to_stage3`).
    pub display_name: String,
    pub roots: Vec<QueryRoot>,
    pub dag: PhysicalASAPDAG,
}

#[derive(Debug, Error)]
pub enum PhysicalASAPOptimizationError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error(transparent)]
    Timing(#[from] ExecutionDataStateError),
    #[error("exact top-k has no Sort + Limit implementation: {0}")]
    ExactTopK(String),
}

/// Stage 2 for one CandidateLogicalASAPDAG: implement every operator
/// (physical operator implementation) and time it with every
/// sub-DAG `NotMaterialized`. `id` and `display_name` are left empty for the caller
/// to name. Sharing between `roots` is preserved: one memo serves the whole
/// workload.
pub fn optimize_physical_asap(
    from_logical: &str,
    roots: &[QueryRoot],
) -> Result<PhysicalCandidate, PhysicalASAPOptimizationError> {
    let mut memo = HashMap::new();
    let implemented = map_roots(roots, &mut |node| implement(node, &mut memo))?;
    let assignment = MaterializationAssignment::with_default(Materialization::NotMaterialized);
    let mut timing = TimingMemo::new();
    let timed = map_roots(&implemented, &mut |node| {
        Ok(apply_materialization_timings(
            node,
            &assignment,
            &mut timing,
        )?)
    })?;
    let dag = compile_physical_asap_workload_with_node_ids(&timed)?.dag;
    Ok(PhysicalCandidate {
        id: String::new(),
        from_logical: from_logical.to_string(),
        display_name: String::new(),
        roots: timed,
        dag,
    })
}

/// `roots` with every operator node they read replaced by `f`'s result.
fn map_roots(
    roots: &[QueryRoot],
    f: &mut impl FnMut(&Rc<OperatorNode>) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError>,
) -> Result<Vec<QueryRoot>, PhysicalASAPOptimizationError> {
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Ok(QueryRoot::Operator(f(node)?)),
            QueryRoot::Scalar(expr) => {
                let mut mapped = HashMap::new();
                for node in expr.operator_refs() {
                    mapped.insert(Rc::as_ptr(node), f(node)?);
                }
                Ok(QueryRoot::Scalar(expr.map_operator_refs(&mut |node| {
                    mapped[&Rc::as_ptr(node)].clone()
                })))
            }
        })
        .collect()
}

/// Each node's implementation.
type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

fn implement(
    node: &Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        implement(child, memo)?;
    }
    let implemented = match node.non_asap() {
        Some(NonASAPOp::Aggregate { measures, .. })
            if measures.iter().any(|m| matches!(m, AggIntent::TopK { .. })) =>
        {
            implement_topk(node, memo)?
        }
        _ => rebuilt(node, memo)?,
    };
    memo.insert(Rc::as_ptr(node), implemented.clone());
    Ok(implemented)
}

/// Exact top-k as `Limit(Sort)` per group, which outputs the ranked rows of
/// the child, under a `Project` restoring the logical `[keys.., topk_k]`
/// output (the ranked value under `topk_k`): a root returns those columns,
/// and a node reading the top-k resolved its columns against them.
fn implement_topk(
    node: &Rc<OperatorNode>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    let unsupported = |why: &str| PhysicalASAPOptimizationError::ExactTopK(why.to_string());
    let Some(NonASAPOp::Aggregate {
        reduction,
        measures,
        filters,
        having,
        child,
        ..
    }) = node.non_asap()
    else {
        unreachable!("implement_topk is called on an Aggregate")
    };
    let Reduction::Reduce(keys) = reduction else {
        return Err(unsupported("per-entity reduction"));
    };
    let [AggIntent::TopK { k, .. }] = measures.as_slice() else {
        return Err(unsupported("measures besides the top-k"));
    };
    if filters.iter().any(Option::is_some) {
        return Err(unsupported("filtered measure"));
    }
    if having.is_some() {
        return Err(unsupported("HAVING"));
    }
    let child = memo[&Rc::as_ptr(child)].clone();
    let value = ranked_column(&child)?;
    let sorted = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Column(value),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: keys.clone(),
            child: child.clone(),
        }))?
        .with_guarantee(child.guarantee.clone()),
    );
    let limited = Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Limit {
            n: Some(*k),
            offset: 0,
            partition_by: keys.clone(),
            child: sorted,
        }))?
        .with_guarantee(node.guarantee.clone()),
    );
    let (ranked, key_fields) = node
        .schema
        .fields
        .split_last()
        .expect("an aggregate outputs its measure");
    let mut cols = key_fields
        .iter()
        .map(|field| {
            let column = resolve_column_ref(&ColumnRef::Named(field.name.clone()), &child.schema)
                .map_err(|e| unsupported(&format!("group key {}: {e}", field.name)))?;
            Ok(ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(column),
            })
        })
        .collect::<Result<Vec<_>, PhysicalASAPOptimizationError>>()?;
    cols.push(ProjectItem {
        alias: Some(ranked.name.clone()),
        expr: ScalarExpr::Column(value),
    });
    Ok(Rc::new(
        OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
            cols,
            qualifier: None,
            child: limited,
        }))?
        .with_guarantee(node.guarantee.clone()),
    ))
}

/// The column exact top-k ranks `child` by. Over a one-measure grouped
/// aggregate (the SQL heavy-hitter shape and PromQL `topk(k, sum by ..)`) it
/// is that measure, the last column after the group keys, whichever Stage 1
/// candidate realizes it: the aggregate itself, or a `SummaryAgg` read by
/// `FinalizeExactAccumulator` or `SummaryEstimate`, under the `Project`
/// restoring its names (a renaming) and a value `HAVING`'s `Filter`. Over per-series rows
/// (PromQL `topk by (..) (k, rows)`) it is the sample value.
fn ranked_column(child: &OperatorNode) -> Result<ColumnId, PhysicalASAPOptimizationError> {
    let mut node = child;
    let reduction = loop {
        match &node.operator {
            // Only a renaming keeps the measure last.
            Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
                if cols.len() == child.schema.fields.len()
                    && cols
                        .iter()
                        .enumerate()
                        .all(|(i, item)| item.expr == ScalarExpr::Column(i)) =>
            {
                node = child
            }
            Operator::NonASAP(NonASAPOp::Filter { child, .. })
            | Operator::ASAP(
                ASAPOp::FinalizeExactAccumulator { child }
                | ASAPOp::SummaryEstimate {
                    summary_input: child,
                    ..
                },
            ) => node = child,
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => break Some(reduction),
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            }) if measures.len() == 1 => break Some(reduction),
            _ => break None,
        }
    };
    match reduction {
        Some(Reduction::Reduce(_)) => Ok(child.schema.fields.len() - 1),
        _ => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).map_err(|e| {
            PhysicalASAPOptimizationError::ExactTopK(format!("no ranked column: {e}"))
        }),
    }
}

/// `node` over its implemented children; the same `Rc` when none changed.
fn rebuilt(
    node: &Rc<OperatorNode>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    let input = |child: &Rc<OperatorNode>| memo[&Rc::as_ptr(child)].clone();
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &input(child)));
    Ok(if changed {
        Rc::new(
            node.with_new_children(|child| input(child))?
                .with_guarantee(node.guarantee.clone()),
        )
    } else {
        node.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::physical_export::PhysicalASAPNodeId;
    use asap_types::ir::physical_export::PhysicalASAPOperatorPayload as Payload;
    use asap_types::ir::ASAPOp;
    use asap_types::post_asap::ExecutionTiming;
    use asap_types::types::AccuracyTarget;

    fn operator_root(candidate: &PhysicalCandidate, i: usize) -> Rc<OperatorNode> {
        match &candidate.roots[i] {
            QueryRoot::Operator(node) => node.clone(),
            QueryRoot::Scalar(_) => panic!("operator root"),
        }
    }

    fn names(node: &OperatorNode) -> Vec<String> {
        node.schema.fields.iter().map(|f| f.name.clone()).collect()
    }

    /// The `Limit` under an exact top-k's restoring `Project`.
    fn topk_limit(project: &OperatorNode) -> Rc<OperatorNode> {
        let NonASAPOp::Project { child, .. } = project.expect_non_asap() else {
            panic!("project, found {}", project.operator.kind_name())
        };
        child.clone()
    }

    /// The single sort key of the `Sort` under the `Limit` at `limit`.
    fn topk_sort_key(limit: &OperatorNode) -> (ScalarExpr, bool) {
        let NonASAPOp::Limit { child: sort, .. } = limit.expect_non_asap() else {
            panic!("limit, found {}", limit.operator.kind_name())
        };
        let NonASAPOp::Sort { keys, .. } = sort.expect_non_asap() else {
            panic!("sort below limit")
        };
        let [key] = keys.as_slice() else {
            panic!("one sort key")
        };
        (key.expr.clone(), key.ascending)
    }

    /// Example 1's shape, `topk by (job) (k, per-series rows)`, sorts each
    /// group by the sample value, descending.
    #[test]
    fn promql_topk_over_series_sorts_by_value_descending() {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::Exact,
        );
        let candidate = optimize_physical_asap("L1", &[root.clone().into()]).unwrap();
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), names(&root));
        let limit = topk_limit(&physical);
        assert_eq!(limit.schema.fields[1].name, "value");
        assert_eq!(topk_sort_key(&limit), (ScalarExpr::Column(1), false));
    }

    /// `topk(k, sum by (job) (..))` sorts by the summed measure.
    #[test]
    fn promql_topk_over_grouped_aggregate_sorts_by_its_measure() {
        let root = lower_promql("topk(5, sum by (job) (m))", AccuracyTarget::Exact);
        let candidate = optimize_physical_asap("L1", &[root.into()]).unwrap();
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), ["topk_5"]);
        let limit = topk_limit(&physical);
        assert_eq!(names(&limit), ["job", "sum"]);
        assert_eq!(topk_sort_key(&limit), (ScalarExpr::Column(1), false));
    }

    /// A node reading an exact top-k keeps resolving the columns it bound
    /// against the logical `[keys.., topk_k]` output: `sum by (job)` still
    /// groups by `job`, not by whatever column now sits at its index.
    #[test]
    fn consumer_of_exact_topk_reads_its_logical_columns() {
        let root = lower_promql(
            "sum by (job) (topk by (job) (10, sum_over_time(m[1m])))",
            AccuracyTarget::Exact,
        );
        let candidate = optimize_physical_asap("L1", &[root.clone().into()]).unwrap();
        let physical = operator_root(&candidate, 0);
        assert_eq!(names(&physical), names(&root));
        let NonASAPOp::Aggregate { child: input, .. } = physical.expect_non_asap() else {
            panic!("sum root")
        };
        let NonASAPOp::Project {
            cols, child: limit, ..
        } = input.expect_non_asap()
        else {
            panic!("project over the top-k rows")
        };
        let read: Vec<_> = cols
            .iter()
            .map(|item| {
                let ScalarExpr::Column(c) = item.expr else {
                    panic!("column")
                };
                (
                    item.alias.clone().unwrap(),
                    limit.schema.fields[c].name.clone(),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("job".to_string(), "job".to_string()),
                ("topk_10".to_string(), "value".to_string())
            ]
        );
        assert_eq!(topk_sort_key(limit), (ScalarExpr::Column(1), false));
    }

    /// An `Aggregate{[TopK]}` that Sort + Limit cannot express (another
    /// measure, `HAVING`) fails, so its candidate is pruned with the reason.
    #[test]
    fn topk_with_extra_measure_or_having_fails() {
        use crate::test_support::{aggregate, metric_scan};
        use asap_types::ir::Predicate;
        use asap_types::pre_asap::expr_ir::ScalarValue;
        let topk = AggIntent::TopK {
            k: 3,
            accuracy: AccuracyTarget::Exact,
        };
        let count = AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        };
        let by_job = || Reduction::by(vec![2]);
        let cases = [
            aggregate(
                by_job(),
                vec![topk.clone(), count],
                vec![],
                None,
                metric_scan(&["job"]),
            ),
            aggregate(
                by_job(),
                vec![topk],
                vec![],
                Some(Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))),
                metric_scan(&["job"]),
            ),
        ];
        for root in cases {
            let error = optimize_physical_asap("L1", &[root.into()]).unwrap_err();
            assert!(
                matches!(error, PhysicalASAPOptimizationError::ExactTopK(_)),
                "{error}"
            );
        }
    }

    /// Exact `topk by (job)` becomes Limit(Sort) partitioned by `job`, and a
    /// child shared by two roots stays one node (one `Rc`, one DAG node).
    #[test]
    fn exact_topk_becomes_sort_then_limit_and_keeps_shared_child() {
        let exact = AccuracyTarget::Exact;
        let topk = lower_promql("topk by (job) (10, sum_over_time(m[1m]))", exact.clone());
        let sum = lower_promql("sum by (job) (sum_over_time(m[1m]))", exact);
        // Share Q2's per-series child with Q1, as Stage 1 sharing would.
        let NonASAPOp::Aggregate { child: shared, .. } = topk.expect_non_asap() else {
            panic!("topk aggregate")
        };
        let sum = Rc::new(sum.with_new_children(|_| shared.clone()).unwrap());
        let candidate = optimize_physical_asap("L1", &[topk.into(), sum.into()]).unwrap();
        let root = |i: usize| match &candidate.roots[i] {
            QueryRoot::Operator(node) => node.clone(),
            QueryRoot::Scalar(_) => panic!("operator root"),
        };

        let NonASAPOp::Limit {
            n: Some(10),
            partition_by,
            child: sort,
            ..
        } = topk_limit(&root(0)).expect_non_asap().clone()
        else {
            panic!("limit under the root")
        };
        assert!(!partition_by.keys().is_empty());
        let NonASAPOp::Sort {
            child: below_sort,
            partition_by: sort_partition,
            ..
        } = sort.expect_non_asap()
        else {
            panic!("sort below limit")
        };
        assert_eq!(*sort_partition, partition_by);
        let NonASAPOp::Aggregate {
            child: below_sum, ..
        } = root(1).expect_non_asap().clone()
        else {
            panic!("sum root")
        };
        assert!(Rc::ptr_eq(below_sort, &below_sum));

        let dag = &candidate.dag;
        let kind = |id: PhysicalASAPNodeId| match &dag
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .payload
        {
            Payload::NonASAP(NonASAPOp::Sort { .. }) => "sort",
            Payload::NonASAP(NonASAPOp::Limit { .. }) => "limit",
            _ => "other",
        };
        let producer_of = |consumer| {
            dag.edges
                .iter()
                .find(|e| e.consumer == consumer)
                .unwrap()
                .producer
        };
        let limit_id = producer_of(dag.roots[0]);
        assert_eq!(kind(limit_id), "limit");
        let sort_id = producer_of(limit_id);
        assert_eq!(kind(sort_id), "sort");
        assert_eq!(producer_of(sort_id), producer_of(dag.roots[1]));
    }

    /// With summaries chosen, every node, the summary build included, runs at
    /// query time.
    #[test]
    fn everything_runs_at_query_time() {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        // A summary (the last candidate) for every target.
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| t.choices.candidates.len() - 1)
            .collect();
        let roots: Vec<_> =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &choice)
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
        let candidate = optimize_physical_asap("L1", &roots).unwrap();
        assert!(candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))));
        assert!(candidate
            .dag
            .nodes
            .iter()
            .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    }
}
