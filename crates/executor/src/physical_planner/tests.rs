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

/// The refresh of a `TimeShift(5m)` materialized at ingestion time over an
/// ingestion-time chain, the shift its compile root: the chain slot is read
/// unbounded, so the refresh is refused ("read unbounded"), whether the
/// shift is the root or another node reads it. How a stored shift stamps
/// its rows is not defined in the executor yet.
#[test]
fn a_stored_time_shift_over_a_chain_reads_the_chain_unbounded() {
    use planner_types::ir::operator::Reduction as IrReduction;
    use planner_types::ir::properties::ExecutionDataState;
    use planner_types::ir::schema::Schema;
    use planner_types::stages::{Lifetime, StorageMedium};
    use planner_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
    let stored = |id: PhysicalASAPNodeId, payload: Payload| PhysicalASAPDAGNode {
        id,
        payload,
        output_state: ExecutionDataState::INGESTION_ROWS,
        lifetime: Lifetime::AcrossEvaluations(StorageMedium::Memory),
        parameters: None,
        output_schema: Schema::default(),
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    };
    let chain = stored(
        0,
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: IrReduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }),
    );
    let shift = stored(
        1,
        Payload::NonASAP(NonASAPOp::TimeShift {
            shift: planner_types::ir::operator::arguments::TimeShift {
                offset_ms: 300_000,
                at: None,
            },
            child: 0,
        }),
    );
    let nodes: BTreeMap<NodeId, &PhysicalASAPDAGNode> = [(0, &chain), (1, &shift)].into();
    let consumers: BTreeMap<NodeId, Vec<NodeId>> = [(0, vec![1])].into();
    let dependencies: BTreeMap<NodeId, Vec<NodeId>> = [(1, vec![0])].into();
    for roots in [BTreeSet::from([1]), BTreeSet::new()] {
        let graph = CompileGraph {
            nodes: &nodes,
            dependencies: &dependencies,
            consumers: &consumers,
            roots: &roots,
        };
        assert_eq!(window_read_above(0, &graph), None);
    }
}

/// A global merge (`group_by: Some([])`) of summary states built from a SQL
/// table's rows reads a `Relation`, so it keeps SQL's one row over no input
/// rows, also when another summary operator sits between the build and the
/// merge: `SummaryMerge([]) ← SummarySubtract ← SummaryAgg ← Scan(t)` and
/// the same with `SummaryJoin`. Over a PromQL series scan the same tree
/// reads an instant vector (no row over an empty vector).
#[test]
fn a_global_merge_reads_the_rows_its_states_were_built_from() {
    use planner_types::ir::operator::arguments::Source;
    use planner_types::ir::operator::Reduction as IrReduction;
    use planner_types::ir::schema::Schema;
    use planner_types::ir::Operator as IrOperator;
    use planner_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
    use std::rc::Rc;
    let node = |operator| Rc::new(OperatorNode::with_schema(operator, Schema::default()));
    for (source, expected) in [
        (
            Source::Table {
                table_ref: "t".into(),
            },
            OperatorResultKind::Relation,
        ),
        (
            Source::TimeSeries { metric: "m".into() },
            OperatorResultKind::InstantVector,
        ),
    ] {
        let scan = node(IrOperator::NonASAP(NonASAPOp::Scan {
            source,
            predicates: vec![],
            schema: Schema::default(),
        }));
        let build = node(IrOperator::ASAP(ASAPOp::SummaryAgg {
            child: scan,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: IrReduction::PerEntity,
            filter: None,
        }));
        let subtract = node(IrOperator::ASAP(ASAPOp::SummarySubtract {
            left: build.clone(),
            right: build.clone(),
        }));
        let join = node(IrOperator::ASAP(ASAPOp::SummaryJoin {
            outer: build.clone(),
            inner: build.clone(),
            key: 0,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        }));
        for states in [subtract, join] {
            let merge = OperatorNode::with_schema(
                IrOperator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![states.clone()],
                    group_by: Some(vec![]),
                }),
                Schema::default(),
            );
            assert_eq!(
                input_rows_kind(&merge),
                expected,
                "{}",
                states.operator.kind_name()
            );
        }
    }
}
