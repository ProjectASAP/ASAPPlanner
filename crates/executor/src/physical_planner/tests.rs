use super::*;

fn empty_dag() -> PhysicalASAPDAG {
    PhysicalASAPDAG {
        nodes: vec![],
        edges: vec![],
        roots: vec![],
        scalar_roots: vec![],
    }
}

/// An empty DAG (no roots) is invalid.
#[test]
fn an_empty_dag_is_invalid() {
    assert!(matches!(
        compile(&empty_dag(), &[], &[]),
        Err(Error::Invalid(_))
    ));
}
