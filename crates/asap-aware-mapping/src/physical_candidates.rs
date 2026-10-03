//! #509 Stage 2 (MVP): physical operator implementation of one logical
//! candidate. No materialization choice is made: every node runs at query
//! time, so each logical candidate yields exactly one physical candidate.
//!
//! The runtime has one implementation per logical operator except exact
//! top-k, which it cannot run as `Aggregate{[TopK]}`. Stage 2 records that
//! choice in the DAG by rewriting it to a per-group sort followed by a
//! per-group limit, the shape the PromQL frontend uses for generic `topk`.
//! A summary needs no rewrite: `SummaryAgg` → `SummaryEstimate` already is
//! build → estimate.
use std::collections::HashMap;
use std::rc::Rc;

use asap_types::ir::export::{compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, ScalarExpr, SchemaDerivationError, SortKey, TimingMemo,
};
use asap_types::post_asap::ExecutionDataStateError;
use asap_types::pre_asap::column_resolution::resolve_column_ref;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::{AggIntent, Reduction};
use thiserror::Error;

/// One Stage 2 candidate, derived from exactly one Stage 1 candidate.
/// `roots` are the timed operator roots (one per query, in workload order)
/// that `dag` exports.
#[derive(Debug, Clone)]
pub struct PhysicalCandidate {
    pub id: String,
    pub from_logical: String,
    pub label: String,
    pub roots: Vec<Rc<OperatorNode>>,
    pub dag: PhysicalASAPDAG,
}

#[derive(Debug, Error)]
pub enum Stage2Error {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error(transparent)]
    Timing(#[from] ExecutionDataStateError),
    #[error("exact top-k has no sortable value column: {0}")]
    NoValueColumn(String),
    #[error("maintained populations run at ingestion time, which Stage 2 does not plan yet")]
    IngestionTimeOnly,
}

/// Implement every operator of one logical candidate and time it at query
/// time. `id` and `label` are left empty for the caller to name. Sharing
/// between `roots` is preserved: one memo serves the whole workload.
pub fn stage2_physical(
    from_logical: &str,
    roots: &[Rc<OperatorNode>],
) -> Result<PhysicalCandidate, Stage2Error> {
    let mut memo = HashMap::new();
    let implemented = roots
        .iter()
        .map(|root| implement(root, &mut memo))
        .collect::<Result<Vec<_>, _>>()?;
    reject_maintained_populations(&implemented)?;
    // The default assignment computes every summary state at query time.
    let assignment = MaterializationAssignment::default();
    let mut timing = TimingMemo::new();
    let timed = implemented
        .iter()
        .map(|root| apply_materialization_timings(root, &assignment, &mut timing))
        .collect::<Result<Vec<_>, _>>()?;
    let dag = compile_physical_asap_workload_with_node_ids(&timed)?.dag;
    Ok(PhysicalCandidate {
        id: String::new(),
        from_logical: from_logical.to_string(),
        label: String::new(),
        roots: timed,
        dag,
    })
}

/// A maintained population always runs at ingestion time, which Stage 2
/// does not plan yet.
fn reject_maintained_populations(roots: &[Rc<OperatorNode>]) -> Result<(), Stage2Error> {
    let maintained = roots.iter().flat_map(OperatorNode::reachable).any(|node| {
        matches!(
            node.operator,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. })
        )
    });
    if maintained {
        return Err(Stage2Error::IngestionTimeOnly);
    }
    Ok(())
}

type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

fn implement(node: &Rc<OperatorNode>, memo: &mut Memo) -> Result<Rc<OperatorNode>, Stage2Error> {
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
                    .map_err(|e| Stage2Error::NoValueColumn(e.to_string()))?;
                let sorted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Sort {
                    keys: vec![SortKey {
                        expr: ScalarExpr::Column(value),
                        ascending: false,
                        nulls_first: false,
                    }],
                    partition_by: keys.clone(),
                    child,
                }))?;
                OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
                    n: Some(*k),
                    offset: 0,
                    partition_by: keys.clone(),
                    child: sorted,
                }))?
            }
            _ => rebuilt(node, memo)?,
        },
        _ => rebuilt(node, memo)?,
    };
    memo.insert(Rc::as_ptr(node), implemented.clone());
    Ok(implemented)
}

/// `node` over its implemented children; the same `Rc` when none changed.
fn rebuilt(node: &Rc<OperatorNode>, memo: &Memo) -> Result<Rc<OperatorNode>, Stage2Error> {
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
    Ok(if changed {
        Rc::new(node.map_children(|child| memo[&Rc::as_ptr(child)].clone())?)
    } else {
        node.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::export::PhysicalASAPOperatorPayload as Payload;
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPNodeId};
    use asap_types::ir::QueryRoot;
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
        let sum = Rc::new(sum.map_children(|_| shared.clone()).unwrap());
        let candidate = stage2_physical("L1", &[topk, sum]).unwrap();

        let NonASAPOp::Limit {
            n: Some(10),
            partition_by,
            child: sort,
            ..
        } = candidate.roots[0].expect_non_asap()
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
        assert_eq!(sort_partition, partition_by);
        let NonASAPOp::Aggregate {
            child: below_sum, ..
        } = candidate.roots[1].expect_non_asap()
        else {
            panic!("sum root")
        };
        assert!(Rc::ptr_eq(below_sort, below_sum));

        let dag = &candidate.dag;
        let kind = |id: PhysicalASAPNodeId| match &dag
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .payload
        {
            Payload::Relational {
                operator: NonASAPOpKind::Sort { .. },
            } => "sort",
            Payload::Relational {
                operator: NonASAPOpKind::Limit { .. },
            } => "limit",
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
        let inventory = crate::logical_candidates::enumerate_local_logical_candidates(vec![(
            0,
            QueryRoot::Operator(root),
        )])
        .unwrap();
        // A summary (the last alternative) for every target.
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| t.alternatives.len() - 1)
            .collect();
        let roots: Vec<_> =
            crate::logical_candidates::compose_logical_candidate(&inventory, &choice)
                .unwrap()
                .into_iter()
                .map(|(_, root)| match root {
                    QueryRoot::Operator(node) => node,
                    QueryRoot::Scalar(_) => panic!("operator root"),
                })
                .collect();
        let candidate = stage2_physical("L1", &roots).unwrap();
        assert!(candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::SummaryAgg { .. })));
        assert!(candidate
            .dag
            .nodes
            .iter()
            .all(|n| n.output_state.timing == ExecutionTiming::QueryTime));
    }
}
