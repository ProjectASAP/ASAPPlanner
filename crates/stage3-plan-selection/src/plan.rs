//! The plan Stage 3 builds from the MILP's optimum as its last step
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 3, "Plan output"): one `PhysicalASAPDAG` node per selected
//! execution ([`candidate_node`], which costing builds too), its inputs
//! from the selected edges, each node's `selected_physical_candidate` set
//! and `retention_ms` its candidate's $h_v$. Several nodes may come from
//! one candidate, one per context it runs in.

use std::collections::{BTreeMap, HashMap};

use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::QueryRoot;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPDAGValidationError, PhysicalASAPNodeId,
};
use asap_types::plan::view_coverage;
use asap_types::stages::{MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId};

use crate::milp::model::{SelectedEdge, SolvedSelection};
use crate::selectable::{Execution, QueryId};

/// Candidate `v` as a plan node with id `id`, its input `i` read from node
/// `input_ids[i]`: its operator, timing, lifetime and θ, its group's schema
/// and, materialized across evaluations, its group's coverage.
/// `retention_ms` is left `None` for the caller to set.
pub(crate) fn candidate_node(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    id: PhysicalASAPNodeId,
    input_ids: &[PhysicalASAPNodeId],
) -> PhysicalASAPDAGNode {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[v.0];
    let logical = memo.candidate(candidate.logical_candidate_id);
    let group = &memo.groups()[candidate.logical_candidate_id.group.0 as usize].group_properties;
    let mut inputs = input_ids.iter();
    let payload = logical
        .expression
        .operator
        .map_children(|_| *inputs.next().expect("one node per input"));
    let lifetime = candidate.physical_properties.lifetime();
    PhysicalASAPDAGNode {
        id,
        output_state: ExecutionDataState {
            timing: candidate.physical_properties.timing(),
            primitive: DataPrimitive::of(&payload, group.result_kind),
        },
        payload,
        lifetime,
        parameters: logical.logical_properties.parameters().cloned(),
        output_schema: group.schema.clone(),
        retention_ms: None,
        selected_physical_candidate: Some(v),
        view_coverage: candidate
            .physical_properties
            .is_across_evaluations()
            .then(|| view_coverage(memo, candidate.logical_candidate_id.group))
            .flatten(),
    }
}

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
    for &SelectedEdge {
        consumer,
        edge,
        producer,
    } in &solved.edges
    {
        producer_of.insert(
            (consumer, physical.edge(edge).input_index_of_consumer),
            producer,
        );
        consumers_of.entry(producer).or_default().push(consumer);
    }
    let input_count = |execution: Execution| physical.input_count(execution.candidate);
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
