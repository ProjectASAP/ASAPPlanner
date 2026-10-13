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
//! A summary needs no rewrite: `SummaryAgg` → `SummaryEstimate` already is
//! build → estimate. Every node keeps the guarantee Stage 1 composed for it.
use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG,
};
use asap_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NonASAPOp, Operator, OperatorNode,
    QueryRoot, ScalarExpr, SchemaDerivationError, SortKey, TimingMemo,
};
use asap_types::post_asap::{ExecutionDataStateError, Materialization};
use asap_types::pre_asap::column_resolution::resolve_column_ref;
use asap_types::pre_asap::expr_ir::ColumnRef;
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
    pub label: String,
    pub roots: Vec<QueryRoot>,
    pub dag: PhysicalASAPDAG,
}

#[derive(Debug, Error)]
pub enum PhysicalASAPOptimizationError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error(transparent)]
    Timing(#[from] ExecutionDataStateError),
    #[error("exact top-k has no sortable value column: {0}")]
    NoValueColumn(String),
}

/// Stage 2 for one CandidateLogicalASAPDAG: implement every operator
/// (physical operator implementation) and time it with every
/// sub-DAG `NotMaterialized`. `id` and `label` are left empty for the caller
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
        label: String::new(),
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
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(keys),
            measures,
            filters,
            having: None,
            child,
            ..
        }) if filters.iter().all(Option::is_none) => match measures.as_slice() {
            [AggIntent::TopK { k, .. }] => {
                let child = memo[&Rc::as_ptr(child)].clone();
                let value = resolve_column_ref(&ColumnRef::SampleValue, &child.schema)
                    .map_err(|e| PhysicalASAPOptimizationError::NoValueColumn(e.to_string()))?;
                let guarantee = child.guarantee.clone();
                let sorted = Rc::new(
                    OperatorNode::new(Operator::NonASAP(NonASAPOp::Sort {
                        keys: vec![SortKey {
                            expr: ScalarExpr::Column(value),
                            ascending: false,
                            nulls_first: false,
                        }],
                        partition_by: keys.clone(),
                        child,
                    }))?
                    .with_guarantee(guarantee),
                );
                Rc::new(
                    OperatorNode::new(Operator::NonASAP(NonASAPOp::Limit {
                        n: Some(*k),
                        offset: 0,
                        partition_by: keys.clone(),
                        child: sorted,
                    }))?
                    .with_guarantee(node.guarantee.clone()),
                )
            }
            _ => rebuilt(node, memo)?,
        },
        _ => rebuilt(node, memo)?,
    };
    memo.insert(Rc::as_ptr(node), implemented.clone());
    Ok(implemented)
}

/// `node` over its implemented children; the same `Rc` when none changed.
fn rebuilt(
    node: &Rc<OperatorNode>,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PhysicalASAPOptimizationError> {
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
    Ok(if changed {
        Rc::new(
            node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?
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
        } = root(0).expect_non_asap().clone()
        else {
            panic!("limit root")
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
        let sort_id = dag
            .edges
            .iter()
            .find(|e| kind(e.producer) == "sort" && e.consumer == dag.roots[0])
            .expect("sort -> limit edge")
            .producer;
        let producer_of = |consumer| {
            dag.edges
                .iter()
                .find(|e| e.consumer == consumer)
                .unwrap()
                .producer
        };
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
