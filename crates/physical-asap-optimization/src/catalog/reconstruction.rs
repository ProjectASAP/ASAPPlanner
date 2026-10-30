//! Reconstruct selected immediate operators, preserving candidate identity.
//!
//! This decoder checks typed connections, not Stage 2 execution legality or
//! query accuracy targets. The caller must run Stage 2 and the plan validator.
use std::collections::HashMap;
use std::rc::Rc;

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::{OperatorNode, QueryRoot};

use asap_types::stages::{
    DagOfSelectedCandidates, PhysicalASAPMemoCandidates, PhysicalCandidateId,
};

/// Enumerate in exactly the traversal order used when rebinding. `children()`
/// puts direct children before scalar references; `map_children` follows fields.
pub fn ordered_inputs(node: &OperatorNode) -> Vec<Rc<OperatorNode>> {
    let mut inputs = Vec::new();
    node.operator.map_children(|child| {
        inputs.push(Rc::clone(child));
        Rc::clone(child)
    });
    inputs
}

/// Open schemas do not promise completeness; an equivalent derived output may
/// strengthen that promise to closed. Supplied uniqueness must imply every
/// required key; additional proven keys are allowed. Every field, configuration
/// parameter and time axis remains exact. Serialization includes sketch error/failure
/// metadata deliberately omitted by some parameter PartialEq implementations.
pub fn output_contract_satisfied(
    expected: &OperatorNode,
    output: &OperatorNode,
) -> Result<bool, String> {
    let mut supplied = output.schema.clone();
    if !expected.schema.closed {
        supplied.closed = false;
    }
    if !expected.schema.unique_keys.iter().all(|required| {
        supplied
            .unique_keys
            .iter()
            .any(|provided| provided.iter().all(|column| required.contains(column)))
    }) {
        return Ok(false);
    }
    supplied.unique_keys = expected.schema.unique_keys.clone();
    Ok(expected.result_kind == output.result_kind
        && expected.schema == supplied
        && serde_json::to_string(&expected.schema).map_err(|error| error.to_string())?
            == serde_json::to_string(&supplied).map_err(|error| error.to_string())?)
}

pub struct ReconstructedCandidates {
    pub outputs: Vec<Rc<OperatorNode>>,
    /// The operator node each selected node became, indexed by
    /// [`SelectedNodeId`](asap_types::stages::SelectedNodeId), with
    /// guarantees recomputed; each a distinct operator node.
    pub(crate) operator_node_of_selected_node: Vec<Rc<OperatorNode>>,
}

/// The nodes of `selected` rebuilt over their selected producers, one
/// operator node each, with guarantees recomposed. Checks the selection's
/// structure, not its cost.
pub fn reconstruct_selected_candidates(
    physical: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
) -> Result<ReconstructedCandidates, String> {
    super::validate_candidates(physical)?;
    selected.validate(physical)?;
    let count = selected.candidate_of_node.len();
    let mut inputs = selected
        .candidate_of_node
        .iter()
        .map(|candidate| {
            vec![
                None;
                physical.candidates[candidate.0]
                    .producer_candidates_per_input
                    .len()
            ]
        })
        .collect::<Vec<_>>();
    let mut consumers = vec![Vec::new(); count];
    for edge in &selected.edges {
        inputs[edge.consumer.0][edge.input] = Some(edge.producer.0);
        consumers[edge.producer.0].push(edge.consumer.0);
    }
    let mut remaining = inputs.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = Vec::new();
    for (node, candidate) in selected.candidate_of_node.iter().enumerate() {
        let template = physical.physical_node(*candidate);
        if template.children().len() != inputs[node].len() {
            return Err(format!(
                "candidate {} template inputs do not match producer requirements",
                candidate.0
            ));
        }
        if remaining[node] == 0 {
            ready.push(node);
        }
    }
    let mut rebuilt: Vec<Option<Rc<OperatorNode>>> = vec![None; count];
    while let Some(node) = ready.pop() {
        let id = selected.candidate_of_node[node].0;
        let template = physical.physical_node(PhysicalCandidateId(id));
        let expected = ordered_inputs(template);
        let mut producers = Vec::with_capacity(inputs[node].len());
        for (input, producer) in inputs[node].iter().enumerate() {
            let producer = rebuilt[producer.unwrap()].as_ref().unwrap();
            if !output_contract_satisfied(&expected[input], producer)? {
                return Err(format!(
                    "candidate {id} input {input} has incompatible schema or state configuration"
                ));
            }
            producers.push(Rc::clone(producer));
        }
        let mut next = 0;
        // Re-derive structural metadata over the selected inputs, preserving
        // only explicit naming overrides. Logical rewrite templates can retain
        // pre-rewrite keys/nullability; copying their entire schema is invalid.
        let rebuilt_node = Rc::new(
            template
                .with_new_children(|_| {
                    let producer = Rc::clone(&producers[next]);
                    next += 1;
                    producer
                })
                .map_err(|e| format!("candidate {id}: {e}"))?
                .with_guarantee(template.guarantee.clone()),
        );
        rebuilt_node
            .validate_structure()
            .map_err(|e| format!("candidate {id}: {e}"))?;
        rebuilt[node] = Some(rebuilt_node);
        for &consumer in &consumers[node] {
            remaining[consumer] -= 1;
            if remaining[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    let rebuilt = rebuilt
        .into_iter()
        .map(|node| node.ok_or_else(|| "selected nodes form a cycle".to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let roots = selected
        .node_of_output_reference
        .iter()
        .map(|node| QueryRoot::Operator(Rc::clone(&rebuilt[node.0])))
        .collect::<Vec<_>>();
    let outputs = recompose_guarantees(&roots)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Ok(node),
            QueryRoot::Scalar(_) => Err("unexpected scalar during operator reconstruction".into()),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let old_outputs = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Rc::clone(node),
            QueryRoot::Scalar(_) => unreachable!("operator references were constructed above"),
        })
        .collect::<Vec<_>>();
    let identities = paired_node_identities(&old_outputs, &outputs)?;
    let operator_node_of_selected_node = rebuilt
        .into_iter()
        .map(|node| {
            identities
                .get(&Rc::as_ptr(&node))
                .cloned()
                .ok_or_else(|| "selected node lost during guarantee recomposition".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReconstructedCandidates {
        outputs,
        operator_node_of_selected_node,
    })
}

/// Match corresponding graph positions without structural interning. Reject
/// splitting or merging identities, and rewrites that change immediate operators.
pub fn paired_node_identities(
    before: &[Rc<OperatorNode>],
    after: &[Rc<OperatorNode>],
) -> Result<HashMap<*const OperatorNode, Rc<OperatorNode>>, String> {
    if before.len() != after.len() {
        return Err("output reference count changed".into());
    }
    let mut identities = HashMap::<*const OperatorNode, Rc<OperatorNode>>::new();
    let mut reverse = HashMap::new();
    let mut pending = before
        .iter()
        .cloned()
        .zip(after.iter().cloned())
        .collect::<Vec<_>>();
    while let Some((old, new)) = pending.pop() {
        if let Some(existing) = identities.get(&Rc::as_ptr(&old)) {
            if !Rc::ptr_eq(existing, &new) {
                return Err("selected node was split into multiple physical nodes".into());
            }
            continue;
        }
        if reverse
            .insert(Rc::as_ptr(&new), Rc::as_ptr(&old))
            .is_some_and(|previous| previous != Rc::as_ptr(&old))
        {
            return Err("distinct selected nodes were merged".into());
        }
        let old_operator = old.operator.map_children(|_| ());
        let new_operator = new.operator.map_children(|_| ());
        // Compare all serialized parameters, including sketch probability and
        // non-finite scalar values. PartialEq rejects an unchanged NaN, while
        // JSON erases the distinction between NaN and either infinity.
        if rmp_serde::to_vec(&old_operator).map_err(|e| e.to_string())?
            != rmp_serde::to_vec(&new_operator).map_err(|e| e.to_string())?
        {
            return Err("operator implementation changed; explicit physical candidate enumeration is required".into());
        }
        let old_children = ordered_inputs(&old);
        let new_children = ordered_inputs(&new);
        if old_children.len() != new_children.len() {
            return Err("candidate input count changed".into());
        }
        pending.extend(old_children.into_iter().zip(new_children));
        identities.insert(Rc::as_ptr(&old), new);
    }
    Ok(identities)
}
