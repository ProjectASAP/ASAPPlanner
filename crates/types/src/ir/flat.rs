//! A shared operator DAG as a flat, serializable list of nodes.
//!
//! In memory, a node's children are `Rc<OperatorNode>`s, and a shared sub-DAG
//! is one `Rc` with several parents. Serializing that tree directly would
//! repeat every shared sub-DAG. [`flatten`] instead gives each distinct node
//! an index and writes its operator with every child, including the nodes
//! read by its scalar expressions, replaced by that index
//! ([`Operator<NodeId>`]). The operator and scalar types are the same ones
//! the planner uses; only the child reference type differs.

use std::collections::HashMap;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::ir::operator::node::{Operator, OperatorNode, OperatorResultKind};
use crate::ir::operator::query::QueryRoot;
use crate::ir::properties::execution_data_state::ExecutionTiming;
use crate::ir::properties::guarantee::AccuracyGuarantee;
use crate::ir::schema::Schema;
use crate::physical::materialization::Materialization;

/// Index of a node in [`FlatDag::nodes`].
pub type NodeId = usize;

/// An [`OperatorNode`] whose children are [`NodeId`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlatNode {
    pub operator: Operator<NodeId>,
    pub(crate) result_kind: OperatorResultKind,
    pub(crate) schema: Schema,
    pub(crate) guarantee: Option<AccuracyGuarantee>,
    pub(crate) timing: Option<ExecutionTiming>,
    pub(crate) materialization: Materialization,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlatDag {
    /// Node `i` is `nodes[i]`. Children come before their parents.
    pub nodes: Vec<FlatNode>,
    /// One root per input root, in input order.
    pub roots: Vec<QueryRoot<NodeId>>,
}

/// Flatten the DAG reachable from `roots`. Each distinct `Rc` (by pointer)
/// becomes one node, so sharing is kept. Also returns the original node for
/// each id.
pub fn flatten(roots: &[QueryRoot]) -> (FlatDag, Vec<Rc<OperatorNode>>) {
    flatten_before(roots, None).expect("flatten without a deadline cannot time out")
}

#[derive(Debug, thiserror::Error)]
#[error("planning deadline exceeded flattening the physical DAG")]
pub(crate) struct FlattenDeadlineExceeded;

/// The same pointer-preserving export, cooperatively bounded while visiting
/// nodes. A timeout returns no partially flattened DAG.
pub(crate) fn flatten_before(
    roots: &[QueryRoot],
    deadline: Option<std::time::Instant>,
) -> Result<(FlatDag, Vec<Rc<OperatorNode>>), FlattenDeadlineExceeded> {
    let mut ids = HashMap::new();
    let mut nodes = Vec::new();
    let mut originals = Vec::new();
    check_deadline(deadline)?;
    let mut error = None;
    let mut id_of = |node: &Rc<OperatorNode>| {
        if error.is_some() {
            return 0;
        }
        match visit(node, &mut ids, &mut nodes, &mut originals, deadline) {
            Ok(id) => id,
            Err(failed) => {
                error = Some(failed);
                0
            }
        }
    };
    let roots = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(id_of(node)),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(expr.map_operator_refs(&mut id_of)),
        })
        .collect();
    if let Some(error) = error {
        return Err(error);
    }
    check_deadline(deadline)?;
    Ok((FlatDag { nodes, roots }, originals))
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<(), FlattenDeadlineExceeded> {
    if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
        Err(FlattenDeadlineExceeded)
    } else {
        Ok(())
    }
}

fn visit(
    node: &Rc<OperatorNode>,
    ids: &mut HashMap<*const OperatorNode, NodeId>,
    nodes: &mut Vec<FlatNode>,
    originals: &mut Vec<Rc<OperatorNode>>,
    deadline: Option<std::time::Instant>,
) -> Result<NodeId, FlattenDeadlineExceeded> {
    check_deadline(deadline)?;
    if let Some(&id) = ids.get(&Rc::as_ptr(node)) {
        return Ok(id);
    }
    let mut error = None;
    let operator = node.operator.map_children(|child| {
        if error.is_some() {
            return 0;
        }
        match visit(child, ids, nodes, originals, deadline) {
            Ok(id) => id,
            Err(failed) => {
                error = Some(failed);
                0
            }
        }
    });
    if let Some(error) = error {
        return Err(error);
    }
    check_deadline(deadline)?;
    let id = nodes.len();
    nodes.push(FlatNode {
        operator,
        result_kind: node.result_kind,
        schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        timing: node.timing,
        materialization: node.materialization,
    });
    originals.push(Rc::clone(node));
    ids.insert(Rc::as_ptr(node), id);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::expr_ir::ScalarValue;
    use crate::ir::schema::{DataType, Field};
    use crate::ir::{NonASAPOp, Predicate, ProjectItem, ScalarExpr};

    fn values() -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
            rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0))]],
            schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
        }))
        .unwrap()
    }

    fn filter(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            child,
        }))
        .unwrap()
    }

    #[test]
    fn bounded_flatten_preserves_identity_and_abandons_partial_export() {
        use std::time::{Duration, Instant};
        let shared = values();
        let roots = vec![
            QueryRoot::Operator(filter(Rc::clone(&shared))),
            QueryRoot::Operator(filter(Rc::clone(&shared))),
        ];
        let (expected, originals) = flatten(&roots);
        let (bounded, bounded_originals) =
            flatten_before(&roots, Some(Instant::now() + Duration::from_secs(1))).unwrap();
        assert_eq!(bounded, expected);
        assert!(originals
            .iter()
            .zip(&bounded_originals)
            .all(|(a, b)| Rc::ptr_eq(a, b)));
        assert!(flatten_before(&roots, Some(Instant::now())).is_err());
        let large: Vec<_> = (0..100_000)
            .map(|_| QueryRoot::Operator(values()))
            .collect();
        assert!(flatten_before(&large, Some(Instant::now() + Duration::from_millis(1))).is_err());
    }

    #[test]
    fn a_shared_node_is_flattened_once() {
        let shared = values();
        let a = filter(Rc::clone(&shared));
        let b = filter(Rc::clone(&shared));
        let (dag, originals) = flatten(&[QueryRoot::Operator(a), QueryRoot::Operator(b)]);

        assert_eq!(dag.nodes.len(), 3);
        assert!(Rc::ptr_eq(&originals[0], &shared));
        assert_eq!(
            dag.roots,
            vec![QueryRoot::Operator(1), QueryRoot::Operator(2)]
        );
        for root in [1, 2] {
            assert_eq!(dag.nodes[root].operator.children(), vec![&0]);
        }
    }

    #[test]
    fn scalar_references_become_node_ids() {
        let v = values();
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("s".into()),
                expr: ScalarExpr::ScalarSubquery(Rc::clone(&v)),
            }],
            qualifier: None,
            child: Rc::clone(&v),
        }))
        .unwrap();
        let (dag, _) = flatten(&[QueryRoot::Operator(project)]);

        assert_eq!(dag.nodes.len(), 2);
        let Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) = &dag.nodes[1].operator
        else {
            panic!("expected a Project");
        };
        assert_eq!(*child, 0);
        assert_eq!(cols[0].expr, ScalarExpr::ScalarSubquery(0));
    }

    #[test]
    fn a_constant_scalar_root_has_no_nodes() {
        let (dag, _) = flatten(&[QueryRoot::Scalar(ScalarExpr::literal_f64(42.0))]);
        assert!(dag.nodes.is_empty());
        assert_eq!(
            dag.roots,
            vec![QueryRoot::Scalar(ScalarExpr::Literal(
                ScalarValue::Float64(42.0)
            ))]
        );
    }

    #[test]
    fn json_round_trips() {
        let (dag, _) = flatten(&[QueryRoot::Operator(filter(values()))]);
        assert!(dag.nodes.iter().all(|n| n.timing.is_none()));
        let json = serde_json::to_string(&dag).unwrap();
        assert_eq!(serde_json::from_str::<FlatDag>(&json).unwrap(), dag);
    }
}
