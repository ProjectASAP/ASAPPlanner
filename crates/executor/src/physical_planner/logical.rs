//! Reconstruct shared operator references from the transport DAG for native lowering.
use super::*;
use planner_types::ir::{ASAPOp, Operator as LogicalOperator, OperatorNode};
use std::rc::Rc;

pub(super) fn restore_before(
    dag: &PhysicalASAPDAG,
    deadline: Option<std::time::Instant>,
) -> Result<BTreeMap<NodeId, Rc<OperatorNode>>, Error> {
    check_compile_deadline(deadline)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let mut done: BTreeMap<NodeId, Rc<OperatorNode>> = BTreeMap::new();
    let mut remaining: Vec<_> = dag.nodes.iter().collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for node in remaining {
            check_compile_deadline(deadline)?;
            if node
                .payload
                .children()
                .iter()
                .any(|child| !done.contains_key(&(**child as u64)))
            {
                next.push(node);
                continue;
            }
            if matches!(
                node.payload,
                LogicalOperator::ASAP(
                    ASAPOp::SummarySubtract { .. }
                        | ASAPOp::SummaryDelete { .. }
                        | ASAPOp::SummaryJoin { .. }
                        | ASAPOp::Extension { .. }
                )
            ) {
                return Err(invalid("reserved ASAP operation has no native lowering"));
            }
            let operator = node
                .payload
                .map_children(|child| Rc::clone(&done[&(*child as u64)]));
            let mut rebuilt = OperatorNode::with_schema(operator, node.output_schema.clone());
            rebuilt.guarantee = node.guarantee.clone();
            rebuilt.timing = Some(node.output_state.timing);
            rebuilt.materialization = node.materialization;
            done.insert(node.id as u64, Rc::new(rebuilt));
        }
        if next.len() == before {
            return Err(invalid("operator DAG is cyclic"));
        }
        remaining = next;
    }
    Ok(done)
}
