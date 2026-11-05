//! The executor's physical planner over every valid timing × lifetime
//! state of one summary build.
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use planner_types::ir::operator::operator_properties::{Reduction, Source};
use planner_types::ir::physical_export::compile_physical_asap_dag;
use planner_types::ir::properties::ExecutionTiming;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{
    DataType, ExactKind, ExactParams, Field, FieldDataType, Schema, SummaryUpdate,
};
use planner_types::ir::{
    apply_physical_properties, ASAPOp, NodeToTimedNodeCache, NonASAPOp, Operator, OperatorNode,
    PhysicalPropertiesAssignment, TimeRangeKind,
};
use planner_types::physical::{KeptProperties, PhysicalProperties};

/// `FinalizeExactAccumulator(SummaryAgg(Sum)(TimeRange 5 min(Scan latency_ms)))`.
fn plan() -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency_ms".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: scan,
    }))
    .unwrap();
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: range,
        family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![]),
        grouping: Default::default(),
        filter: None,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: state,
    }))
    .unwrap()
}

/// What the physical planner makes of the build in each valid state: a
/// state kept across evaluations (either timing, either medium) is a
/// materialized view slot the deployment binds; every other state is
/// computed in the query run (a `OneEvaluation` result once for the run's
/// readers). A summary build not kept at ingestion time is not built by
/// rule: the timing pass runs an unkept build at query time.
#[test]
fn physical_planner_over_every_valid_state() {
    for properties in PhysicalProperties::every_valid() {
        let root = plan();
        let mut assignment = PhysicalPropertiesAssignment::default();
        if let Some(kept) = properties.kept() {
            assignment.set(root.children()[0], kept);
        }
        let timed = apply_physical_properties(&root, &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
        let mut dag = compile_physical_asap_dag(&timed).unwrap();
        let views = dag.set_retention(&HashMap::new(), &HashMap::new()).unwrap();
        let compiled = asap_executor::physical_planner::compile(&dag, &views, &[3])
            .unwrap_or_else(|error| panic!("{properties:?}: {error}"));
        let build = compiled.operator_name(2);
        match properties {
            PhysicalProperties::Kept(KeptProperties::AcrossEvaluationsAtIngestion(_))
            | PhysicalProperties::Kept(KeptProperties::AcrossEvaluationsAtQueryTime(_)) => {
                assert_eq!(build, Some("MaterializedView"), "{properties:?}");
            }
            PhysicalProperties::NotKept(ExecutionTiming::QueryTime)
            | PhysicalProperties::Kept(KeptProperties::ForOneEvaluationAtQueryTime(_)) => {
                assert_ne!(build, Some("MaterializedView"), "{properties:?}");
                assert!(build.is_some(), "{properties:?}");
            }
            PhysicalProperties::NotKept(ExecutionTiming::IngestionTime) => {
                // Not assignable to a summary build: it lands at query time.
                assert_eq!(
                    dag.nodes[2].physical_properties(),
                    Ok(PhysicalProperties::NotKept(ExecutionTiming::QueryTime))
                );
            }
        }
    }
}
