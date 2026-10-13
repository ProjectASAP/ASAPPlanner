//! Compilation rejects Planner node IDs outside the range its helper IDs avoid.
mod common;
use asap_physical_operators::physical_planner::compile;
use common::compile_physical_asap_dag;
use planner_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr};
use planner_types::pre_asap::{DataType, Field, ScalarValue, Schema};
use std::collections::BTreeMap;

/// A deserialized DAG may carry any ID; one above u32 is an error, not a
/// collision with helper IDs or a panic.
#[test]
fn node_ids_above_u32_are_rejected() {
    let values = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Int64(1))]],
        schema: Schema::new(vec![Field::plain("x", DataType::Int64, false)]),
    }))
    .unwrap();
    let filter = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        child: values,
    }))
    .unwrap();
    let mut dag = compile_physical_asap_dag(&filter).unwrap();
    let root = dag.roots[0] as u64;
    assert!(compile(&dag, BTreeMap::new(), &[root]).is_ok());

    let shift = 1usize << 32;
    for node in &mut dag.nodes {
        node.id += shift;
        node.payload = node.payload.map_children(|child| child + shift);
    }
    for edge in &mut dag.edges {
        edge.producer += shift;
        edge.consumer += shift;
    }
    for root in &mut dag.roots {
        *root += shift;
    }
    dag.validate().unwrap();
    let Err(error) = compile(&dag, BTreeMap::new(), &[root + shift as u64]) else {
        panic!("an ID above u32 compiled");
    };
    assert!(error.to_string().contains("exceeds u32"), "{error}");
}
