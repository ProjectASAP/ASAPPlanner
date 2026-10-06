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

use super::node::{Operator, OperatorNode, OperatorResultKind};
use super::query::QueryRoot;
use super::summary_coverage::SummaryCoverage;
use crate::post_asap::execution_data_state::ExecutionTiming;
use crate::post_asap::guarantee::ResultGuarantee;
use crate::pre_asap::schema::Schema;

/// Index of a node in [`FlatDag::nodes`].
pub type NodeId = usize;

/// An [`OperatorNode`] whose children are [`NodeId`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlatNode {
    pub operator: Operator<NodeId>,
    pub result_kind: OperatorResultKind,
    pub schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
    pub timing: Option<ExecutionTiming>,
    #[serde(default)]
    pub coverage: Option<SummaryCoverage>,
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
    let mut ids = HashMap::new();
    let mut nodes = Vec::new();
    let mut originals = Vec::new();
    let mut id_of = |node: &Rc<OperatorNode>| visit(node, &mut ids, &mut nodes, &mut originals);
    let roots = roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(id_of(node)),
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(expr.map_operator_refs(&mut id_of)),
        })
        .collect();
    (FlatDag { nodes, roots }, originals)
}

fn visit(
    node: &Rc<OperatorNode>,
    ids: &mut HashMap<*const OperatorNode, NodeId>,
    nodes: &mut Vec<FlatNode>,
    originals: &mut Vec<Rc<OperatorNode>>,
) -> NodeId {
    if let Some(&id) = ids.get(&Rc::as_ptr(node)) {
        return id;
    }
    let operator = node
        .operator
        .map_children(|child| visit(child, ids, nodes, originals));
    let id = nodes.len();
    nodes.push(FlatNode {
        operator,
        result_kind: node.result_kind,
        schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        timing: node.timing,
        coverage: node.coverage.clone(),
    });
    originals.push(Rc::clone(node));
    ids.insert(Rc::as_ptr(node), id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{NonASAPOp, Predicate, ProjectItem, ScalarExpr};
    use crate::pre_asap::expr_ir::ScalarValue;
    use crate::pre_asap::schema::{DataType, Field};

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
