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
    for roots in [BTreeSet::from([1]), BTreeSet::new()] {
        assert_eq!(window_read_above(0, &roots, &consumers, &nodes), None);
    }
}
