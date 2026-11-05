//! The executor's physical planner over every valid timing × lifetime
//! state of one summary build.
use planner_types::physical::Medium;
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
    apply_physical_properties, ASAPOp, NonASAPOp, Operator, OperatorNode,
    PhysicalPropertiesAssignment, TimeRangeKind,
};
use planner_types::physical::{Lifetime, PhysicalProperties};

/// `FinalizeExactAccumulator(SummaryMerge(SummaryAgg(Sum)(TimeRange 5 min(Scan latency_ms))))`.
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
    let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state],
        newest_tumbling_windows: None,
        group_by: None,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: merge,
    }))
    .unwrap()
}

/// The build under the merge under `root`.
fn build_of(root: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    root.children()[0].children()[0]
}

/// What the physical planner makes of the build in each valid state: a
/// state materialized across evaluations (either timing, either medium) is
/// a materialized view slot the deployment binds; a query-time state that
/// is not materialized across evaluations is computed in the query run (a
/// `OneEvaluation` result once for the run's consumers). Not materialized at
/// ingestion time, the build sits under the merge materialized at ingestion
/// time and is computed in that view's ingestion run.
#[test]
fn physical_planner_over_every_valid_state() {
    use ExecutionTiming::{IngestionTime, QueryTime};
    for properties in [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime]
        .into_iter()
        .flat_map(|timing| {
            [
                Lifetime::None,
                Lifetime::OneEvaluation(Medium::Memory),
                Lifetime::OneEvaluation(Medium::Disk),
                Lifetime::AcrossEvaluations(Medium::Memory),
                Lifetime::AcrossEvaluations(Medium::Disk),
            ]
            .map(|lifetime| PhysicalProperties::new(timing, lifetime))
        })
        // Every valid state: the pairs `new` accepts.
        .filter_map(Result::ok)
    {
        let root = plan();
        let mut assignment = PhysicalPropertiesAssignment::default();
        match (properties.timing(), properties.lifetime()) {
            // The build gets no entry: it runs in the ingestion run of the
            // merge above it, which is materialized at ingestion time.
            (IngestionTime, Lifetime::None) => assignment.set(
                root.children()[0],
                PhysicalProperties::new(
                    ExecutionTiming::IngestionTime,
                    Lifetime::AcrossEvaluations(Medium::Memory),
                )
                .expect("a valid timing and lifetime"),
            ),
            _ => assignment.set(build_of(&root), properties),
        }
        let timed = apply_physical_properties(&root, &assignment).unwrap();
        let mut dag = compile_physical_asap_dag(&root, &timed).unwrap();
        let views = dag.set_retention(&HashMap::new(), &HashMap::new()).unwrap();
        let id_of = |summary_merge: bool| {
            dag.nodes
                .iter()
                .find(|node| match &node.payload {
                    Operator::ASAP(ASAPOp::SummaryAgg { .. }) => !summary_merge,
                    Operator::ASAP(ASAPOp::SummaryMerge { .. }) => summary_merge,
                    _ => false,
                })
                .expect("the plan has a summary build and a merge")
                .id
        };
        let (build_id, merge_id) = (id_of(false), id_of(true));
        let compiled =
            asap_executor::physical_planner::compile(&dag, &views, &[dag.roots[0] as u64])
                .unwrap_or_else(|error| panic!("{properties:?}: {error}"));
        let build = compiled.operator_name(build_id as u64);
        let computed_in_the_query_run = || {
            assert_ne!(build, Some("MaterializedView"), "{properties:?}");
            assert!(build.is_some(), "{properties:?}");
        };
        match (properties.timing(), properties.lifetime()) {
            (IngestionTime, Lifetime::AcrossEvaluations(_)) => {
                assert_eq!(build, Some("MaterializedView"), "{properties:?}");
            }
            (QueryTime, Lifetime::AcrossEvaluations(_)) => {
                assert_eq!(build, Some("MaterializedView"), "{properties:?}");
            }
            (QueryTime, Lifetime::None) => computed_in_the_query_run(),
            (QueryTime, Lifetime::OneEvaluation(_)) => computed_in_the_query_run(),
            (IngestionTime, Lifetime::None) => {
                let node = dag.nodes.iter().find(|node| node.id == build_id).unwrap();
                assert_eq!(
                    PhysicalProperties::new(node.output_state.timing, node.lifetime),
                    Ok(properties)
                );
                // The query reads the merge's view; its ingestion run
                // computes the build.
                assert_eq!(
                    compiled.operator_name(merge_id as u64),
                    Some("MaterializedView")
                );
                let refresh =
                    asap_executor::physical_planner::compile(&dag, &[], &[merge_id as u64])
                        .unwrap();
                let build = refresh.operator_name(build_id as u64);
                assert!(
                    build.is_some_and(|name| name != "MaterializedView"),
                    "{build:?}"
                );
            }
            (IngestionTime, Lifetime::OneEvaluation(_)) => {
                unreachable!("rejected by PhysicalProperties::new")
            }
        }
    }
}
