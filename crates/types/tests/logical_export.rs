//! Logical transport must accept phase-free plans before materialization.
use asap_types::{
    ir::{NonASAPOp, Operator, OperatorNode},
    pre_asap::Schema,
};

#[test]
fn logical_export_accepts_unassigned_timing() {
    let root = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![]],
        schema: Schema::lifted(vec![], None),
    }))
    .unwrap();
    assert!(root.timing.is_none());
    assert!(asap_types::ir::export::compile_logical_asap_dag(&root).is_ok());
}

use asap_types::{
    ir::export::{
        compile_logical_asap_dag_with_node_ids, EdgeRole, LogicalASAPDAGDocument,
        LogicalASAPDAGValidationError, LogicalASAPNodeId, LogicalASAPOperatorPayload,
    },
    ir::operator_properties::Reduction,
    ir::{ASAPOp, OperatorResultKind, ProjectItem, ScalarExpr},
    post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, ScalarValue, Source},
};
use std::rc::Rc;

fn values() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(1.0))]],
        schema: Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
    }))
    .unwrap()
}

/// Scalar subqueries contribute real edges; repeated references export one producer.
#[test]
fn scalar_dependencies_share_one_exported_producer() {
    let child = values();
    let root = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![ProjectItem {
            alias: Some("result".into()),
            expr: ScalarExpr::ScalarSubquery(child.clone()),
        }],
        qualifier: None,
        child: child.clone(),
    }))
    .unwrap();
    let compiled = compile_logical_asap_dag_with_node_ids(&root).unwrap();
    compiled.dag.validate().unwrap();
    assert_eq!(compiled.dag.nodes.len(), 2);
    assert_eq!(compiled.dag.edges.len(), 2);
    assert!(compiled
        .dag
        .edges
        .iter()
        .any(|edge| edge.role == EdgeRole::ScalarRef));
    let id = compiled.node_ids.node_id(&child).unwrap();
    assert!(Rc::ptr_eq(
        compiled.node_ids.operator_node(id).unwrap(),
        &child
    ));
    let document = LogicalASAPDAGDocument::new(compiled.dag);
    let json = serde_json::to_string(&document).unwrap();
    for physical_metadata in [
        "output_state",
        "data_state",
        "timing",
        "retention",
        "window",
    ] {
        assert!(
            !json.contains(physical_metadata),
            "logical JSON contains {physical_metadata}"
        );
    }
    let decoded: LogicalASAPDAGDocument = serde_json::from_str(&json).unwrap();
    assert_eq!(document, decoded);
    decoded.validate().unwrap();
}

/// Summary state identity survives logical merge export without a phase assignment.
#[test]
fn merged_summary_preserves_typed_state() {
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: values(),
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: (0..2)
            .map(|start| {
                let coverage = asap_types::ir::summary_coverage::SummaryCoverage {
                    source: Source::Table {
                        table_ref: "values".into(),
                    },
                    regions: vec![asap_types::ir::summary_coverage::CoverageRegion {
                        time_ms: Some(start..start + 1),
                        population: Default::default(),
                    }],
                };
                std::rc::Rc::new((*state).clone().with_coverage(coverage).unwrap())
            })
            .collect(),
    }))
    .unwrap();
    let dag = asap_types::ir::export::compile_logical_asap_dag(&root).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.nodes.len(), 4);
    let root_id = dag.root.operator_refs()[0];
    let merged = &dag.nodes[root_id.0 as usize];
    assert_eq!(merged.result_kind, OperatorResultKind::State);
    assert_eq!(merged.output_schema, root.schema);
    assert_eq!(merged.coverage, root.coverage);
    assert!(matches!(
        merged.payload,
        LogicalASAPOperatorPayload::SummaryMerge
    ));
    // Transport rejects a summary producer whose required coverage was dropped.
    let mut stripped = dag.clone();
    let producer = stripped
        .nodes
        .iter_mut()
        .find(|node| matches!(node.payload, LogicalASAPOperatorPayload::SummaryAgg { .. }))
        .unwrap();
    producer.coverage = None;
    let id = producer.id;
    assert!(matches!(
        stripped.validate(),
        Err(LogicalASAPDAGValidationError::InvalidCoverage(bad)) if bad == id
    ));
}

/// Malformed wire graphs fail transport integrity checks rather than reaching execution.
#[test]
fn malformed_transport_is_rejected() {
    let dag = asap_types::ir::export::compile_logical_asap_dag(&values()).unwrap();
    let mut document = LogicalASAPDAGDocument::new(dag.clone());
    document.schema_version = 99;
    assert!(matches!(
        document.validate(),
        Err(LogicalASAPDAGValidationError::UnsupportedVersion(99))
    ));
    let mut duplicate = dag.clone();
    duplicate.nodes.push(dag.nodes[0].clone());
    assert!(matches!(
        duplicate.validate(),
        Err(LogicalASAPDAGValidationError::DuplicateNode(_))
    ));
    let mut missing = dag.clone();
    missing.root = asap_types::ir::export::LogicalASAPQueryRoot::Operator(LogicalASAPNodeId(9));
    assert!(matches!(
        missing.validate(),
        Err(LogicalASAPDAGValidationError::MissingNode(_))
    ));
    let mut unreachable = dag.clone();
    let mut extra = dag.nodes[0].clone();
    extra.id = LogicalASAPNodeId(1);
    unreachable.nodes.push(extra);
    assert!(matches!(
        unreachable.validate(),
        Err(LogicalASAPDAGValidationError::UnreachableNode(_))
    ));
    let mut json = serde_json::to_value(LogicalASAPDAGDocument::new(dag)).unwrap();
    json["dag"]["nodes"][0]["output_state"] = serde_json::json!({"timing":"ingestion_time"});
    assert!(serde_json::from_value::<LogicalASAPDAGDocument>(json).is_err());
}

/// Standalone constants need no fake relation, while scalar subqueries retain their producer DAG.
#[test]
fn standalone_scalar_roots_roundtrip_without_synthetic_operators() {
    use asap_types::ir::{export::compile_logical_asap_query, QueryRoot};
    for root in [
        QueryRoot::Scalar(ScalarExpr::literal_f64(42.0)),
        QueryRoot::Scalar(ScalarExpr::ScalarSubquery(values())),
    ] {
        let expected = root.as_operator().is_some();
        assert!(!expected);
        let dag = compile_logical_asap_query(&root).unwrap();
        dag.validate().unwrap();
        let expected_nodes = match root {
            QueryRoot::Scalar(ScalarExpr::ScalarSubquery(_)) => 1,
            _ => 0,
        };
        assert_eq!(dag.nodes.len(), expected_nodes);
        let document = LogicalASAPDAGDocument::new(dag);
        let decoded: LogicalASAPDAGDocument =
            serde_json::from_str(&serde_json::to_string(&document).unwrap()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(document, decoded);
    }
}
