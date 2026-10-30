//! Reconstruct selected immediate operators, preserving candidate identity.
//!
//! This decoder checks typed connections, not Stage 2 execution legality or
//! query accuracy targets. The caller must run Stage 2 and the plan validator.
use std::collections::HashMap;
use std::rc::Rc;

use asap_accuracy::propagation::recompose_guarantees;
use asap_types::ir::{OperatorNode, QueryRoot};

use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, SelectedCandidateGraph};

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
    /// Recomputed guarantees and the same globally unique candidate identity.
    pub(crate) nodes_by_candidate: Vec<Option<Rc<OperatorNode>>>,
}

/// The selected candidates of `physical` rebuilt over their selected
/// producers, with guarantees recomposed. Checks the selection's structure,
/// not its cost.
pub fn reconstruct_selected_candidates(
    physical: &PhysicalASAPMemoCandidates,
    selected: &SelectedCandidateGraph,
) -> Result<ReconstructedCandidates, String> {
    super::validate_candidates(physical)?;
    super::validate_selected_graph(physical, selected)?;
    let count = physical.candidates.len();
    let mut inputs = physical
        .candidates
        .iter()
        .map(|candidate| vec![None; candidate.producer_candidates_per_input.len()])
        .collect::<Vec<_>>();
    let mut consumers = vec![Vec::new(); count];
    for connection in &selected.connections {
        inputs[connection.consumer.0][connection.requirement] = Some(connection.producer);
        consumers[connection.producer.0].push(connection.consumer.0);
    }
    let mut remaining = inputs.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = Vec::new();
    for &PhysicalCandidateId(id) in &selected.selected_candidates {
        let template = physical.physical_node(PhysicalCandidateId(id));
        if template.children().len() != inputs[id].len() {
            return Err(format!(
                "candidate {id} template inputs do not match producer requirements"
            ));
        }
        if remaining[id] == 0 {
            ready.push(id);
        }
    }
    let mut rebuilt: Vec<Option<Rc<OperatorNode>>> = vec![None; count];
    while let Some(id) = ready.pop() {
        let template = physical.physical_node(PhysicalCandidateId(id));
        let expected = ordered_inputs(template);
        let mut producers = Vec::with_capacity(inputs[id].len());
        for (position, producer) in inputs[id].iter().enumerate() {
            let producer = rebuilt[producer.unwrap().0].as_ref().unwrap();
            if !output_contract_satisfied(&expected[position], producer)? {
                return Err(format!("candidate {id} input {position} has incompatible schema or state configuration"));
            }
            producers.push(Rc::clone(producer));
        }
        let mut position = 0;
        // Re-derive structural metadata over the selected inputs, preserving
        // only explicit naming overrides. Logical rewrite templates can retain
        // pre-rewrite keys/nullability; copying their entire schema is invalid.
        let node = Rc::new(
            template
                .with_new_children(|_| {
                    let producer = Rc::clone(&producers[position]);
                    position += 1;
                    producer
                })
                .map_err(|e| format!("candidate {id}: {e}"))?
                .with_guarantee(template.guarantee.clone()),
        );
        node.validate_structure()
            .map_err(|e| format!("candidate {id}: {e}"))?;
        rebuilt[id] = Some(node);
        for &consumer in &consumers[id] {
            remaining[consumer] -= 1;
            if remaining[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    let mut roots = vec![None; physical.output_reference_count()];
    for root in &selected.query_roots {
        roots[root.query_root] = Some(QueryRoot::Operator(Rc::clone(
            rebuilt[root.candidate.0].as_ref().unwrap(),
        )));
    }
    let roots = roots.into_iter().map(Option::unwrap).collect::<Vec<_>>();
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
    let nodes_by_candidate = rebuilt
        .into_iter()
        .map(|node| {
            node.map(|node| {
                identities
                    .get(&Rc::as_ptr(&node))
                    .cloned()
                    .ok_or_else(|| "candidate lost during guarantee recomposition".to_string())
            })
            .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReconstructedCandidates {
        outputs,
        nodes_by_candidate,
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
                return Err("selected candidate was split into multiple physical instances".into());
            }
            continue;
        }
        if reverse
            .insert(Rc::as_ptr(&new), Rc::as_ptr(&old))
            .is_some_and(|previous| previous != Rc::as_ptr(&old))
        {
            return Err("distinct selected candidates were merged".into());
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
