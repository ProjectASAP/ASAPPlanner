//! The plan Stage 3 builds from the baseline MILP's optimum as its last
//! step (concepts doc, section 3, "Plan output"): one `PhysicalASAPDAG`
//! node per selected candidate, its inputs from the selected edges, each
//! node's `selected_physical_candidate` set.

use std::collections::{BTreeMap, HashMap};

use asap_types::ir::QueryRoot;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGValidationError, PhysicalASAPNodeId,
};
use asap_types::stages::{
    MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::cost::{candidate_node, input_count, retention_need_ms};
use crate::milp::model::{QueryId, SolvedSelection};

/// The plan of `solved`, whose output `j` answers `outputs[j]` (a query and
/// the group its root, or one operator its scalar root, reads). Nodes are
/// numbered producers first. A node that retains rows (materialized, or a
/// query-time `Scan`) has `retention_ms` the most its selected consumers
/// read back ([`retention_need_ms`]), and at least the range-evaluation
/// span of a query it answers directly.
pub(crate) fn plan_of(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    solved: &SolvedSelection,
    outputs: &[(QueryId, MemoGroupId)],
) -> Result<PhysicalASAPDAG, PhysicalASAPDAGValidationError> {
    let mut producer_of: HashMap<(PhysicalCandidateId, usize), PhysicalCandidateId> =
        HashMap::new();
    let mut consumers_of: BTreeMap<PhysicalCandidateId, Vec<PhysicalEdgeId>> = BTreeMap::new();
    for &id in &solved.edges {
        let edge = physical.edge(id);
        producer_of.insert(
            (edge.consumer_candidate_id, edge.input_index_of_consumer),
            edge.producer_candidate_id,
        );
        consumers_of
            .entry(edge.producer_candidate_id)
            .or_default()
            .push(id);
    }
    let consumers = |v: PhysicalCandidateId| consumers_of.get(&v).into_iter().flatten().copied();
    let readers_of =
        |v: PhysicalCandidateId| consumers(v).map(|edge| physical.edge(edge).consumer_candidate_id);
    let inputs = |v: PhysicalCandidateId| -> Vec<PhysicalCandidateId> {
        (0..input_count(physical, v))
            .map(|i| producer_of[&(v, i)])
            .collect()
    };
    // Producers first: Kahn's order over the selected edges, which (E)
    // keeps acyclic.
    let mut waiting: HashMap<PhysicalCandidateId, usize> = solved
        .candidates
        .iter()
        .map(|&v| (v, input_count(physical, v)))
        .collect();
    let mut ready: Vec<PhysicalCandidateId> = solved
        .candidates
        .iter()
        .rev()
        .copied()
        .filter(|v| waiting[v] == 0)
        .collect();
    let mut node_of: HashMap<PhysicalCandidateId, PhysicalASAPNodeId> = HashMap::new();
    let mut order = Vec::with_capacity(solved.candidates.len());
    while let Some(v) = ready.pop() {
        node_of.insert(v, order.len());
        order.push(v);
        for edge in consumers(v) {
            let consumer = physical.edge(edge).consumer_candidate_id;
            let left = waiting.get_mut(&consumer).expect("a selected consumer");
            *left -= 1;
            if *left == 0 {
                ready.push(consumer);
            }
        }
    }
    if order.len() != solved.candidates.len() {
        return Err(PhysicalASAPDAGValidationError::Cycle);
    }
    // The span of the queries each candidate answers directly.
    let mut output_span_ms: HashMap<PhysicalCandidateId, u64> = HashMap::new();
    for (&(query, _), &v) in outputs.iter().zip(&solved.outputs) {
        let span_ms = entries
            .get(query.0)
            .and_then(|entry| entry.range_evaluation)
            .map_or(0, |range| range.span_ms);
        let known = output_span_ms.entry(v).or_default();
        *known = (*known).max(span_ms);
    }
    let nodes = order
        .iter()
        .enumerate()
        .map(|(id, &v)| {
            let input_ids: Vec<PhysicalASAPNodeId> =
                inputs(v).iter().map(|producer| node_of[producer]).collect();
            let mut node = candidate_node(physical, v, id, &input_ids);
            if node.retains_rows() {
                node.retention_ms = retention_need_ms(physical, entries, v, readers_of)
                    .map(|need_ms| need_ms.max(output_span_ms.get(&v).copied().unwrap_or(0)));
            }
            node
        })
        .collect();
    let output_node: HashMap<(QueryId, MemoGroupId), PhysicalASAPNodeId> = outputs
        .iter()
        .zip(&solved.outputs)
        .map(|(&output, v)| (output, node_of[v]))
        .collect();
    let mut roots = Vec::new();
    let mut scalar_roots = Vec::new();
    for (query, memo_query) in physical
        .logical_candidates
        .memo
        .queries()
        .iter()
        .enumerate()
    {
        let node = |group: &MemoGroupId| output_node[&(QueryId(query), *group)];
        match &memo_query.root {
            QueryRoot::Operator(group) => roots.push(node(group)),
            QueryRoot::Scalar(expression) => scalar_roots.push((
                query,
                expression.map_operator_refs(&mut |group| node(group)),
            )),
        }
    }
    PhysicalASAPDAG::new(nodes, roots, scalar_roots)
}
