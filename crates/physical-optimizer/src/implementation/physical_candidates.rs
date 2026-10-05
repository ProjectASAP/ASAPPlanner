//! #509 Stage 2: physical operator implementation of one logical candidate,
//! and its materialization choices ([`crate::materialization`]). Each
//! logical candidate yields one physical candidate per choice of
//! ingestion-time (down-closed) and kept summaries, all query time first.
//!
//! The runtime has one implementation per logical operator except exact
//! top-k, which it cannot run as `Aggregate{[TopK]}`. Stage 2 records that
//! choice in the DAG by rewriting it to a per-group sort followed by a
//! per-group limit, the shape the PromQL frontend uses for generic `topk`.
//! A summary needs no rewrite: `SummaryAgg` → `SummaryEstimate` already is
//! build → estimate.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG,
};
use asap_types::ir::properties::ExecutionDataStateError;
use asap_types::ir::scalar::resolve_column_ref;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::{
    apply_materialization_timings, split_shared_by_phase, ASAPOp, MaterializationAssignment,
    NonASAPOp, Operator, OperatorNode, ScalarExpr, SchemaDerivationError, SortKey, TimingMemo,
};
use asap_types::workload::{DataWorkload, RootDemand};
use thiserror::Error;

use crate::materialization::{Choice, Choices, MaterializationSpace, MAX_PHYSICAL_PER_LOGICAL};

/// One Stage 2 candidate, derived from exactly one Stage 1 candidate.
/// `roots` are the timed operator roots (one per query, in workload order)
/// that `dag` exports.
#[derive(Debug, Clone)]
pub struct PhysicalCandidate {
    pub id: String,
    pub from_logical: String,
    pub label: String,
    /// Which summaries run at ingestion time or are kept, e.g. "ingestion
    /// time: Kll ×5 panes" or "query time, kept: Kll ×5 panes"; empty when
    /// everything runs at query time, recomputed at each evaluation.
    pub materialization: String,
    pub roots: Vec<Rc<OperatorNode>>,
    pub dag: PhysicalASAPDAG,
}

/// Stage 2's physical candidates of one logical candidate.
#[derive(Debug, Clone)]
pub struct Stage2Candidates {
    /// All query time first.
    pub candidates: Vec<PhysicalCandidate>,
    /// `false` when there were more than [`MAX_PHYSICAL_PER_LOGICAL`]
    /// materialization choices and they were searched greedily.
    pub exhaustive: bool,
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
    #[error("a summary assigned to ingestion time reads query-time work")]
    NotMaintainable,
}

/// Implement every operator of one logical candidate and enumerate its
/// materialization choices: one candidate per choice of ingestion-time
/// (down-closed) and kept summaries, all query time first. `id` and `label` are left
/// empty for the caller to name. Sharing between `roots` is preserved: one
/// memo serves the whole workload, and a node shared by an ingestion-time and
/// a query-time consumer is copied per phase.
///
/// `demand[i]` is the demand of `roots[i]`. Above
/// [`MAX_PHYSICAL_PER_LOGICAL`] choices, the choices are searched greedily
/// by `score` (lower is better; `None`: not admissible): starting from all
/// query time, move the unit (to ingestion time or kept) whose move improves
/// the score most, until none does. The candidates on that path are returned and flagged not
/// exhaustive.
pub fn stage2_physical(
    from_logical: &str,
    roots: &[Rc<OperatorNode>],
    demand: &[RootDemand],
    data: &DataWorkload,
    score: &dyn Fn(&PhysicalCandidate) -> Option<f64>,
) -> Result<Stage2Candidates, Stage2Error> {
    let mut memo = HashMap::new();
    let implemented = roots
        .iter()
        .map(|root| implement(root, &mut memo))
        .collect::<Result<Vec<_>, _>>()?;
    reject_maintained_populations(&implemented)?;
    let space = MaterializationSpace::new(&implemented, demand, data);
    let build = |set: &Choices| materialize(from_logical, &implemented, &space, set);
    // All query time must build; an ingestion-time choice that the timing
    // rules reject is not a candidate.
    let base = build(&Choices::new())?;
    if let Some(sets) = space.down_closed_sets(MAX_PHYSICAL_PER_LOGICAL) {
        let mut candidates = vec![base];
        candidates.extend(sets.iter().skip(1).filter_map(|set| build(set).ok()));
        return Ok(Stage2Candidates {
            candidates,
            exhaustive: true,
        });
    }
    let mut set = Choices::new();
    let mut best = score(&base);
    let mut candidates = vec![base];
    loop {
        let step = (0..space.units.len())
            .flat_map(|u| [(u, Choice::IngestionTime), (u, Choice::Kept)])
            .filter(|&(u, choice)| space.can_choose(&set, u, choice))
            .filter_map(|(u, choice)| {
                let mut next = set.clone();
                next.insert(u, choice);
                let candidate = build(&next).ok()?;
                let value = score(&candidate)?;
                Some((value, next, candidate))
            })
            .filter(|(value, ..)| best.is_none_or(|best| *value < best))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((value, next, candidate)) = step else {
            break;
        };
        best = Some(value);
        set = next;
        candidates.push(candidate);
    }
    Ok(Stage2Candidates {
        candidates,
        exhaustive: false,
    })
}

/// Time the implemented roots with the summaries of `set` at ingestion time
/// or kept, and mark the kept ones in the exported DAG.
fn materialize(
    from_logical: &str,
    implemented: &[Rc<OperatorNode>],
    space: &MaterializationSpace,
    set: &Choices,
) -> Result<PhysicalCandidate, Stage2Error> {
    let (roots, assignment): (Vec<_>, MaterializationAssignment) =
        split_shared_by_phase(implemented, &space.assignment(set));
    let mut timing = TimingMemo::new();
    let timed = roots
        .iter()
        .map(|root| apply_materialization_timings(root, &assignment, &mut timing))
        .collect::<Result<Vec<_>, _>>()?;
    let compiled = compile_physical_asap_workload_with_node_ids(&timed)?;
    let kept: HashSet<_> = roots
        .iter()
        .flat_map(OperatorNode::reachable)
        .filter(|node| assignment.is_kept(node))
        .filter_map(|node| compiled.node_ids.node_id(timing.timed(&node)?))
        .collect();
    let mut dag = compiled.dag;
    for node in &mut dag.nodes {
        node.kept = kept.contains(&node.id);
    }
    // A summary over query-time work stays at query time whatever the
    // assignment says; such a set is not a distinct candidate.
    let assigned: usize = set
        .iter()
        .filter(|(_, choice)| **choice == Choice::IngestionTime)
        .map(|(&u, _)| space.units[u].summaries.len())
        .sum();
    let maintained = dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { .. })
            ) && !n.output_state.timing.is_query_time()
        })
        .count();
    if maintained != assigned {
        return Err(Stage2Error::NotMaintainable);
    }
    Ok(PhysicalCandidate {
        id: String::new(),
        from_logical: from_logical.to_string(),
        label: String::new(),
        materialization: space.label(set),
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
        Rc::new(node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?)
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
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::ir::QueryRoot;
    use asap_types::types::AccuracyTarget;

    /// The all-query-time candidate, for data with no ingestion.
    fn all_query_time(from: &str, roots: &[Rc<OperatorNode>]) -> PhysicalCandidate {
        stage2_physical(from, roots, &[], &DataWorkload::default(), &|_| None)
            .unwrap()
            .candidates
            .remove(0)
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
        let candidate = all_query_time("L1", &[topk, sum]);

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
        let inventory =
            asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates(
                vec![(0, QueryRoot::Operator(root))],
                &Default::default(),
            )
            .unwrap();
        // A summary (the last alternative) for every target.
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| t.alternatives.len() - 1)
            .collect();
        let roots: Vec<_> =
            asap_logical_optimizer::pass1::logical_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect();
        let candidate = all_query_time("L1", &roots);
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
