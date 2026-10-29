//! A DAG's materialized views and their retention
//! (`asap_types::ir::materialized_view`): two one-minute KLL panes of `m`
//! kept at ingestion, merged into a view refreshed on first read.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::materialized_view::{InstallableDagError, MaterializedViewRefresh};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::TimeShift;
use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPDAG};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NodeToTimedNodeCache,
    NonASAPOp, Operator, OperatorNode, TimeRangeKind,
};
use asap_types::physical::{Materialization, Medium};

/// A KLL of `m`'s values over `(T - shift - 1 min, T - shift]`.
fn pane(scan: &Rc<OperatorNode>, shift_ms: i64) -> Rc<OperatorNode> {
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: shift_ms,
            at: None,
        },
        child: Rc::clone(scan),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: range,
        family: FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![2]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}

/// The panes at offsets 0 and 1 min kept at ingestion, their merge kept
/// across evaluations, exported: `(dag, merge's node id)`.
fn panes_and_kept_merge() -> (PhysicalASAPDAG, usize) {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    let panes = [pane(&scan, 0), pane(&scan, 60_000)];
    let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: panes.to_vec(),
        newest_panes: None,
        group_by: None,
    }))
    .unwrap();
    let mut assignment = MaterializationAssignment::default();
    for pane in &panes {
        assignment.set(pane, Materialization::MatAtIngestion(Medium::Memory));
    }
    assignment.set(
        &merge,
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    );
    let timed =
        apply_materialization_timings(&merge, &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let merge_id = dag.roots[0];
    (dag, merge_id)
}

/// A view without `retention_ms` (a DAG before installation) has no
/// materialized views to list.
#[test]
fn a_view_without_retention_is_refused() {
    let (dag, _) = panes_and_kept_merge();
    assert!(matches!(
        dag.materialized_views(),
        Err(InstallableDagError::ViewWithoutRetention { .. })
    ));
}

/// The merge, refreshed on first read for each step of a one-hour range,
/// is kept for its two minutes plus the hour; each pane it reads is kept
/// that long too, since the merge may be refreshed for its oldest step.
/// The panes read `m` at ingestion, so no raw data is kept for queries.
#[test]
fn retention_propagates_from_a_view_refreshed_on_first_read_to_its_inputs() {
    let (mut dag, merge_id) = panes_and_kept_merge();
    let spans = HashMap::from([(merge_id, 3_600_000)]);
    dag.set_retention(&HashMap::new(), &spans, None).unwrap();
    let views = dag.materialized_views().unwrap();
    let (merge, panes) = views.split_last().unwrap();
    assert_eq!(merge.node_id, merge_id);
    assert_eq!(merge.refresh, MaterializedViewRefresh::OnFirstRead);
    assert_eq!(merge.retention_ms, 120_000 + 3_600_000);
    assert_eq!(panes.len(), 2);
    for pane in panes {
        assert_eq!(pane.refresh, MaterializedViewRefresh::AtIngestion);
        assert_eq!(pane.retention_ms, merge.retention_ms);
        let node = dag.nodes.iter().find(|n| n.id == pane.node_id).unwrap();
        assert_eq!(node.retention_ms, Some(pane.retention_ms));
    }
    assert_eq!(dag.raw_data_retention().unwrap(), []);
}

/// A view kept for less than its data reaches back (the older pane, which
/// reaches 2 minutes back, kept for 1 minute) is refused.
#[test]
fn a_view_kept_shorter_than_its_reach_is_refused() {
    let (mut dag, _) = panes_and_kept_merge();
    let views = dag
        .set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let older = views
        .iter()
        .find(|v| v.inputs.raw_data_windows.first().map(|w| w.end_offset_ms) == Some(60_000))
        .unwrap()
        .node_id;
    dag.nodes
        .iter_mut()
        .find(|n| n.id == older)
        .unwrap()
        .retention_ms = Some(60_000);
    assert_eq!(
        dag.materialized_views(),
        Err(InstallableDagError::ViewRetentionTooShort { node_id: older })
    );
}

/// An expired deadline stops the derivation.
#[test]
fn the_derivation_stops_at_its_deadline() {
    let (mut dag, _) = panes_and_kept_merge();
    assert_eq!(
        dag.set_retention(
            &HashMap::new(),
            &HashMap::new(),
            Some(std::time::Instant::now())
        ),
        Err(InstallableDagError::DeadlineExceeded)
    );
}
