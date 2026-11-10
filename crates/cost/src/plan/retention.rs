//! How long a plan keeps each tumbling window chain and which windows it
//! retains, read from the queries each node reaches.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::plan::execution_demand::query_starts;
use crate::{node_map, NodeMap};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::stages::Lifetime;
use asap_types::workload::RangeEvaluation;

/// For each tumbling-window chain of `dag` materialized across
/// evaluations, how far back from the newest evaluation its completed
/// windows must be retained, in milliseconds. `reach` is [`query_reach`] of
/// `dag`; `range_evaluations[q]` is query `q`'s (none when missing).
pub fn tumbling_window_retention_ms(
    dag: &PhysicalASAPDAG,
    reach: &[HashSet<PhysicalASAPNodeId>],
    range_evaluations: &[Option<RangeEvaluation>],
) -> BTreeMap<PhysicalASAPNodeId, u64> {
    let nodes = node_map(dag);
    dag.nodes
        .iter()
        .filter_map(|node| match (&node.payload, node.lifetime) {
            (
                Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
                Lifetime::AcrossEvaluations(_),
            ) => {
                let _ = (&nodes, reach, range_evaluations);
                todo!("#1207 PR 7: a chain's retention, max(offset + range) over the paths that read it")
            }
            _ => None,
        })
        .collect()
}

/// The tumbling-window retention of every materialized tumbling-window chain of `dag`
/// ([`tumbling_window_retention_ms`] over [`query_reach`]), for a plan whose query
/// `q` has range evaluation `range_evaluations[q]` (none when missing):
/// what Stage 2's realizer records for a realized plan.
pub fn tumbling_window_retention_ms_of_plan(
    dag: &PhysicalASAPDAG,
    range_evaluations: &[Option<RangeEvaluation>],
) -> BTreeMap<PhysicalASAPNodeId, u64> {
    tumbling_window_retention_ms(dag, &query_reach(dag, &node_map(dag)), range_evaluations)
}

/// The panes a chain in `retention_ms` ([`tumbling_window_retention_ms`]) keeps
/// completed: its retention over its pane width, at least its pane count.
pub fn retained_tumbling_windows(
    node: &asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode,
    retention_ms: &BTreeMap<PhysicalASAPNodeId, u64>,
) -> u64 {
    match (&node.payload, retention_ms.get(&node.id)) {
        (
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }),
            Some(&retention),
        ) => (retention / tumbling_window_width_ms.max(&1)).max(tumbling_windows_of(node)),
        _ => tumbling_windows_of(node),
    }
}

/// A node's tumbling windows: a tumbling-window chain's count, 1 for any other node.
pub fn tumbling_windows_of(node: &asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
            todo!("#1207 PR 7: the windows a chain retains (h_v)")
        }
        _ => 1,
    }
}

/// The nodes each query reaches at query time, in workload order: its walk
/// from its roots, stopping at (and including) ingestion-time nodes.
pub fn query_reach(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode>,
) -> Vec<HashSet<PhysicalASAPNodeId>> {
    query_starts(dag)
        .into_iter()
        .map(|mut stack| {
            let mut seen = HashSet::new();
            while let Some(id) = stack.pop() {
                if seen.insert(id)
                    && nodes[&id].output_state.timing != ExecutionTiming::IngestionTime
                {
                    stack.extend(
                        dag.edges
                            .iter()
                            .filter(|e| e.consumer == id)
                            .map(|e| e.producer),
                    );
                }
            }
            seen
        })
        .collect()
}

/// The window an ingestion-time grouping `node` maintains: a tumbling-window chain's
/// tumbling windows, or the time range below it (through filters, projections and
/// time shifts). `None` when there is none.
pub fn maintained_window_ms(
    node: PhysicalASAPNodeId,
    nodes: &NodeMap<'_>,
    producers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
) -> Option<u64> {
    match &nodes[&node].payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => {
            todo!("#1207 PR 7: the window a chain maintains, its retention (h_v)")
        }
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
            return Some(range.as_millis() as u64)
        }
        _ => {}
    }
    let producer = *producers_of.get(&node)?.first()?;
    match &nodes[&producer].payload {
        Payload::NonASAP(
            NonASAPOp::TimeRange { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::TimeShift { .. },
        ) => maintained_window_ms(producer, nodes, producers_of),
        _ => None,
    }
}
