//! The plan Stage 3 builds from the MILP's optimum as its last step
//! (concepts doc, section 3, "Plan output"): one `PhysicalASAPDAG` node
//! per selected execution, its inputs from the selected edges, each node's
//! `selected_physical_candidate` set. Several nodes may come from one
//! candidate, one per context it runs in.

use std::collections::{BTreeMap, HashMap};

use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPDAGValidationError, PhysicalASAPNodeId,
};
use asap_types::ir::QueryRoot;
use asap_types::stages::{MemoGroupId, PhysicalASAPMemoCandidates};

use crate::cost::{candidate_node, input_count};
use crate::milp::execution_contexts::Execution;
use crate::milp::model::{QueryId, SolvedSelection};

/// The plan of `solved`, whose output `j` answers `outputs[j]` (a query and
/// the group its root, or one operator its scalar root, reads). Nodes are
/// numbered producers first. A node whose candidate has $h_v$ retains that
/// much (`retention_ms`); one that retains rows a consumer reads unbounded
/// has `retention_ms` `None` and keeps every row.
pub(crate) fn plan_of(
    physical: &PhysicalASAPMemoCandidates,
    solved: &SolvedSelection,
    outputs: &[(QueryId, MemoGroupId)],
) -> Result<PhysicalASAPDAG, PhysicalASAPDAGValidationError> {
    let mut producer_of: HashMap<(Execution, usize), Execution> = HashMap::new();
    let mut consumers_of: BTreeMap<Execution, Vec<Execution>> = BTreeMap::new();
    for &(consumer, edge, producer) in &solved.edges {
        producer_of.insert(
            (consumer, physical.edge(edge).input_index_of_consumer),
            producer,
        );
        consumers_of.entry(producer).or_default().push(consumer);
    }
    let input_count = |execution: Execution| input_count(physical, execution.candidate);
    // Producers first: Kahn's order over the selected edges, which (E)
    // keeps acyclic.
    let mut waiting: HashMap<Execution, usize> = solved
        .executions
        .iter()
        .map(|&execution| (execution, input_count(execution)))
        .collect();
    let mut ready: Vec<Execution> = solved
        .executions
        .iter()
        .rev()
        .copied()
        .filter(|execution| waiting[execution] == 0)
        .collect();
    let mut node_of: HashMap<Execution, PhysicalASAPNodeId> = HashMap::new();
    let mut order = Vec::with_capacity(solved.executions.len());
    while let Some(execution) = ready.pop() {
        node_of.insert(execution, order.len());
        order.push(execution);
        for consumer in consumers_of.get(&execution).into_iter().flatten() {
            let left = waiting.get_mut(consumer).expect("a selected consumer");
            *left -= 1;
            if *left == 0 {
                ready.push(*consumer);
            }
        }
    }
    if order.len() != solved.executions.len() {
        return Err(PhysicalASAPDAGValidationError::Cycle);
    }
    let nodes = order
        .iter()
        .enumerate()
        .map(|(id, &execution)| {
            let input_ids: Vec<PhysicalASAPNodeId> = (0..input_count(execution))
                .map(|i| node_of[&producer_of[&(execution, i)]])
                .collect();
            let mut node = candidate_node(physical, execution.candidate, id, &input_ids);
            node.retention_ms = solved.retention_ms.get(&execution.candidate).copied();
            node
        })
        .collect();
    let output_node: HashMap<(QueryId, MemoGroupId), PhysicalASAPNodeId> = outputs
        .iter()
        .zip(&solved.outputs)
        .map(|(&output, execution)| (output, node_of[execution]))
        .collect();
    let mut roots = Vec::new();
    let mut scalar_roots = Vec::new();
    for (query, memo_query) in physical.logical_candidates.memo.queries.iter().enumerate() {
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
