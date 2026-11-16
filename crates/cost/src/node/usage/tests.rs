//! Tests of [`super::node_resource_usage_and_cost`].

use std::time::Duration;

use asap_types::ir::properties::ExecutionDataState;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::Field;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::plan::time_window::{InputTimeAccess, TimeIndexedPlan};
use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};

use super::*;
use crate::read_back_ms;

fn node(payload: Payload) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id: 0,
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        lifetime: Lifetime::None,
        parameters: None,
        output_schema: Schema::default(),
        retention_ms: None,
        selected_physical_candidate: None,
        view_coverage: None,
    }
}

fn produced<'a>(
    node: &'a PhysicalASAPDAGNode,
    usage: &NodeResourceUsageAndCost<'a>,
) -> ProducerOutput<'a> {
    usage.producer_output(&node.payload, &node.output_schema)
}

/// A merge of the five 60 s windows a 5m `TimeRange` selects from a
/// chain of 1000 states per window reads 5000 states and outputs one
/// state per group, 1000, not one per state read.
#[test]
fn a_merge_over_a_chain_s_windows_outputs_one_state_per_group() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: 0,
        summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 60_000,
    }));
    let chain_output = ProducerOutput {
        payload: &chain.payload,
        output_schema: &chain.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 16_000,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Exact(&ExactKind::Sum),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: Some(60_000),
    };
    let windows = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let windows_usage =
        node_resource_usage_and_cost(&windows, &[chain_output], (1, 1.0), None, &data, &settings)
            .unwrap();
    assert_eq!(windows_usage.output.rows, 5_000);
    assert_eq!(windows_usage.tumbling_windows, 5);
    let merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    let merge_usage = node_resource_usage_and_cost(
        &merge,
        &[produced(&windows, &windows_usage)],
        (1, 1.0),
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(merge_usage.output.rows, 1_000);
    assert_eq!(merge_usage.output.bytes, 16_000);
    assert!(
        merge_usage
            .cost
            .detail
            .starts_with("merge 5000 states into 1000;"),
        "{}",
        merge_usage.cost.detail
    );
}

/// o11y q21's shape, `sum(rate(m[5m] offset 1h))` over a chain of 60 s
/// windows: a `TimeShift` by 1 h between the chain and the 5m
/// `TimeRange`, or above the `TimeRange`, changes nothing; nor does a
/// `Filter` or a `Project`, which keep each row's window
/// (`Operator::keeps_input_window`). The range selects five windows of
/// 1000 states, and the merge over them reads 5000 states into 1000, as
/// with nothing between (it read 1000, one window).
#[test]
fn an_operator_keeping_its_input_window_over_a_chain_or_its_windows_selects_what_the_range_does() {
    use asap_types::ir::operator::TimeShift;
    use asap_types::ir::scalar::{Predicate, ProjectItem, ScalarExpr, ScalarValue};
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: 0,
        summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 60_000,
    }));
    let shift = node(Payload::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 3_600_000,
            at: None,
        },
        child: 0,
    }));
    let filter = node(Payload::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        child: 0,
    }));
    let project = node(Payload::NonASAP(NonASAPOp::Project {
        cols: vec![ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(0),
        }],
        child: 0,
    }));
    let range = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    // The merge's cost detail and output over `path`, costed bottom-up
    // from the chain's one window of 1000 states.
    let merge_over = |path: &[&PhysicalASAPDAGNode]| -> (String, EdgeStatistics) {
        let mut below = ProducerOutput {
            payload: &chain.payload,
            output_schema: &chain.output_schema,
            output: EdgeStatistics {
                rows: 1_000,
                bytes: 16_000,
            },
            sources: BTreeSet::new(),
            family: StateFamily::Exact(&ExactKind::Sum),
            unchanged_columns: BTreeSet::new(),
            tumbling_windows: 1,
            chain_tumbling_window_width_ms: Some(60_000),
        };
        for &step in path {
            let usage =
                node_resource_usage_and_cost(step, &[below], (1, 1.0), None, &data, &settings)
                    .unwrap();
            below = produced(step, &usage);
        }
        let usage =
            node_resource_usage_and_cost(&merge, &[below], (1, 1.0), None, &data, &settings)
                .unwrap();
        (usage.cost.detail, usage.output)
    };
    let direct = merge_over(&[&range]);
    assert!(
        direct.0.starts_with("merge 5000 states into 1000;"),
        "{}",
        direct.0
    );
    for keeping in [&shift, &filter, &project] {
        for path in [[keeping, &range], [&range, keeping]] {
            assert_eq!(merge_over(&path), direct, "{:?}", keeping.payload);
        }
    }
}

/// A merge over a time range of a DDSketch chain's windows reads DDSketch
/// states, though the time range itself has no family: the merge's θ names
/// it, so each of the 5,000 states read costs a DDSketch merge (2,500
/// operations), not an exact accumulator's (230).
#[test]
fn a_merge_over_a_chain_s_windows_is_costed_by_the_chain_s_family() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let windows = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let windows_output = ProducerOutput {
        payload: &windows.payload,
        output_schema: &windows.output_schema,
        output: EdgeStatistics {
            rows: 5_000,
            bytes: 5_000 * 4_096,
        },
        sources: BTreeSet::new(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 5,
        chain_tumbling_window_width_ms: None,
    };
    let mut merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    merge.parameters = Some(SketchParams::DDSketch { alpha: 0.01 });
    let merge_usage =
        node_resource_usage_and_cost(&merge, &[windows_output], (1, 1.0), None, &data, &settings)
            .unwrap();
    assert!(matches!(
        merge_usage.family,
        StateFamily::Sketch(SketchParams::DDSketch { .. })
    ));
    assert_eq!(
        merge_usage.cpu_ops_per_second,
        5_000.0 * crate::node::operations_per_unit::DDSKETCH_MERGE_OPERATIONS_PER_STATE
    );
}

/// A merge over a time range of a Rate chain's windows reads Rate
/// accumulator states, though the time range has no family and the merge
/// no θ: its output's state column names the family, so each of the 5,000
/// states read costs a counter-increase merge (350 operations), not a
/// plain exact accumulator's (230).
#[test]
fn a_merge_over_a_rate_chain_s_windows_is_costed_by_the_rate_accumulator() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let windows = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let windows_output = ProducerOutput {
        payload: &windows.payload,
        output_schema: &windows.output_schema,
        output: EdgeStatistics {
            rows: 5_000,
            bytes: 5_000 * asap_types::summary_type::EXACT_ACCUMULATOR_STATE_BYTES,
        },
        sources: BTreeSet::new(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 5,
        chain_tumbling_window_width_ms: None,
    };
    let mut merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    merge.output_schema = Schema::new(vec![Field::new(
        "state",
        FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        false,
    )]);
    let merge_usage =
        node_resource_usage_and_cost(&merge, &[windows_output], (1, 1.0), None, &data, &settings)
            .unwrap();
    assert_eq!(merge_usage.family, StateFamily::Exact(&ExactKind::Rate));
    assert_eq!(
        merge_usage.cpu_ops_per_second,
        5_000.0 * crate::node::operations_per_unit::COUNTER_MERGE_OPERATIONS_PER_STATE
    );
}

/// A query-time chain reads its scan's lookback and keeps each row in its
/// window's range, as a `TimeRange` over the same rows does, besides
/// updating the row's state. Example: a sum in 60 s windows over 36,000
/// raw rows, retaining five windows of one group, costs 36,000 × (40
/// update + 12 range) operations plus five emitted states × 2,500.
#[test]
fn a_query_time_chain_pays_the_time_range_over_the_rows_it_reads() {
    use crate::node::operations_per_unit::{
        CHAIN_EMIT_OPERATIONS_PER_STATE, EXACT_UPDATE_OPERATIONS_PER_ROW,
        TIME_RANGE_OPERATIONS_PER_ROW,
    };
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let scan = node(Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".to_string(),
        },
        predicates: vec![],
        schema: Schema::default(),
    }));
    let rows = ProducerOutput {
        payload: &scan.payload,
        output_schema: &scan.output_schema,
        output: EdgeStatistics {
            rows: 36_000,
            bytes: 36_000 * 16,
        },
        sources: BTreeSet::new(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
    };
    let chain = PhysicalASAPDAGNode {
        retention_ms: Some(300_000),
        ..node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![]),
            filter: None,
            tumbling_window_width_ms: 60_000,
        }))
    };
    let usage =
        node_resource_usage_and_cost(&chain, &[rows], (1, 1.0), None, &data, &settings).unwrap();
    assert_eq!(
        usage.cpu_ops_per_second,
        36_000.0 * (EXACT_UPDATE_OPERATIONS_PER_ROW + TIME_RANGE_OPERATIONS_PER_ROW)
            + 5.0 * CHAIN_EMIT_OPERATIONS_PER_STATE
    );
}

/// A query-time node holds its state only
/// while it computes: bytes × its work per execution × its executions
/// per second, with no query latency. A KLL state of 2 MB whose own
/// work takes 3 ms, run 0.1 times per second, holds 2,000,000 × 0.003 ×
/// 0.1 = 600 byte·s per second; here, a `OneEvaluation` merge run 0.1
/// times per second.
#[test]
fn query_time_memory_is_held_only_while_the_node_computes() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let input = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let states = ProducerOutput {
        payload: &input.payload,
        output_schema: &input.output_schema,
        output: EdgeStatistics {
            rows: 5_000,
            bytes: 80_000,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Exact(&ExactKind::Sum),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
    };
    let merge = PhysicalASAPDAGNode {
        lifetime: Lifetime::OneEvaluation(StorageMedium::Memory),
        ..node(Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: None,
        }))
    };
    let usage =
        node_resource_usage_and_cost(&merge, &[states], (1, 0.1), None, &data, &settings).unwrap();
    let held: u64 = usage
        .cost
        .detail
        .split("holds ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|bytes| bytes.parse().ok())
        .expect("the held bytes are in the detail");
    assert!(held > 0);
    let expected = held as f64 * usage.work_ms_per_execution / 1_000.0 * 0.1;
    assert!(
        (usage.average_memory_bytes - expected).abs() <= 1e-9 * expected,
        "{} vs {expected}",
        usage.average_memory_bytes
    );
}

/// A query-time `Scan` keeps what its
/// `retention_ms` covers but reads, per execution, only the most its
/// consumers read (no range-evaluation span): under a `TimeRange` of 1 s
/// over 1,000 rows per second it reads 1,000 rows while it keeps
/// 3,601,000 (a 1 h range query with step 1 s over an instant
/// selector).
#[test]
fn a_query_time_scan_reads_what_its_consumers_read_and_keeps_its_retention() {
    use asap_types::ir::schema::{DataType, Field};
    use asap_types::workload::DurationMs;
    let settings = CostInputs::default();
    let mut data = DataWorkload::sampled_every(["x".to_string()], DurationMs(1_000));
    for facts in data.sources.values_mut() {
        facts.series_count = Some(1_000);
    }
    let scan = PhysicalASAPDAGNode {
        retention_ms: Some(3_601_000),
        output_schema: Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ]),
        ..node(Payload::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "x".into() },
            predicates: vec![],
            schema: Schema::default(),
        }))
    };
    let instant = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(1),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    // The scan (0) read by the range (1).
    struct ScanReadByRange<'a>([&'a PhysicalASAPDAGNode; 2]);
    impl TimeIndexedPlan for ScanReadByRange<'_> {
        type Node = usize;
        type Child = usize;
        fn readers_of(&self, node: usize) -> Vec<usize> {
            if node == 0 {
                vec![1]
            } else {
                vec![]
            }
        }
        fn inputs_of(&self, node: usize) -> Vec<usize> {
            if node == 1 {
                vec![0]
            } else {
                vec![]
            }
        }
        fn input_time_access(&self, node: usize) -> InputTimeAccess {
            let node = self.0[node];
            InputTimeAccess::of(&node.payload, ExecutionTiming::QueryTime, node.lifetime)
        }
        fn current_time_read_needs_every_row(&self, node: usize) -> bool {
            node == 0
        }
        fn operator_of(&self, node: usize) -> &Payload {
            &self.0[node].payload
        }
    }
    let read_ms = read_back_ms(&ScanReadByRange([&scan, &instant]), 0, |_| 0);
    assert_eq!(read_ms, Some(1_000));
    let usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), read_ms, &data, &settings).unwrap();
    assert_eq!(usage.output.rows, 1_000);
    let (_, kept_bytes) = usage.raw_scan.expect("a query-time scan keeps raw data");
    assert_eq!(kept_bytes, usage.output.bytes * 3_601);
}

/// Raw rows materialized at ingestion time behind an offset (a
/// `TimeShift` by 90 s over the arriving rows) keep what their
/// `retention_ms` covers, its offset included (a 5-minute read is retained
/// for 390 s, [`read_back_ms`]), at the source's arrival rate: 1,000 rows
/// per second retained 390 s keep 390,000 rows, not one second's 1,000; a
/// 690 s retention keeps 690,000.
#[test]
fn an_ingestion_time_view_of_raw_rows_keeps_the_rows_its_retention_covers() {
    use asap_types::ir::schema::{DataType, Field};
    use asap_types::workload::DurationMs;
    let settings = CostInputs::default();
    let mut data = DataWorkload::sampled_every(["m".to_string()], DurationMs(1_000));
    for facts in data.sources.values_mut() {
        facts.series_count = Some(1_000);
    }
    let scan = node(Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::default(),
    }));
    let one_second_of_rows = ProducerOutput {
        payload: &scan.payload,
        output_schema: &scan.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 64_000,
        },
        sources: ["m"].into(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
    };
    let row_schema = Schema::new(vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ]);
    let kept_bytes = |retention_ms: u64| {
        let shift = PhysicalASAPDAGNode {
            output_state: ExecutionDataState::INGESTION_ROWS,
            lifetime: Lifetime::AcrossEvaluations(StorageMedium::Memory),
            retention_ms: Some(retention_ms),
            output_schema: row_schema.clone(),
            ..node(Payload::NonASAP(NonASAPOp::TimeShift {
                shift: asap_types::ir::operator::arguments::TimeShift {
                    offset_ms: 90_000,
                    at: None,
                },
                child: 0,
            }))
        };
        node_resource_usage_and_cost(
            &shift,
            std::slice::from_ref(&one_second_of_rows),
            (1, 1.0),
            None,
            &data,
            &settings,
        )
        .unwrap()
        .peak_memory_bytes
    };
    let row_width_bytes = row_bytes(&row_schema);
    assert!(row_width_bytes > 0);
    assert_eq!(kept_bytes(390_000), 390_000 * row_width_bytes);
    assert_eq!(kept_bytes(690_000), 690_000 * row_width_bytes);
}

/// A view materialized across evaluations without a retention is read
/// unbounded (here `sum_over_time(m[5m] offset -5m)`: a `TimeShift` by
/// -5 m over the arriving rows, which no retention covers), so nothing
/// bounds what it keeps and it cannot be installed: it is not costed,
/// rather than charged for one output and selected as the cheapest.
#[test]
fn a_view_without_retention_is_not_costed() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let scan = node(Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::default(),
    }));
    let one_second_of_rows = ProducerOutput {
        payload: &scan.payload,
        output_schema: &scan.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 64_000,
        },
        sources: BTreeSet::new(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
    };
    let shift = PhysicalASAPDAGNode {
        output_state: ExecutionDataState::INGESTION_ROWS,
        lifetime: Lifetime::AcrossEvaluations(StorageMedium::Memory),
        retention_ms: None,
        ..node(Payload::NonASAP(NonASAPOp::TimeShift {
            shift: asap_types::ir::operator::arguments::TimeShift {
                offset_ms: -300_000,
                at: None,
            },
            child: 0,
        }))
    };
    let usage = node_resource_usage_and_cost(
        &shift,
        std::slice::from_ref(&one_second_of_rows),
        (1, 1.0),
        None,
        &data,
        &settings,
    );
    assert!(matches!(
        usage,
        Err(AnalyticalCostError::MissingOrZero("retention_ms"))
    ));
}

/// A view over an ingestion-time chain of 60 s windows is refreshed once
/// per chain window, not once per second: `TimeShift(1h)` over the chain
/// is refreshed on the chain's 60 s window (its refresh window,
/// `RefreshWindow::computed_from`, is the chain's), and,
/// retained for 1 h 5 m (its 5-minute reader and its offset), keeps 65
/// windows' outputs and the one being built, as many as the chain below
/// it, not 3,901 seconds' worth.
#[test]
fn a_view_over_an_ingestion_time_chain_keeps_one_output_per_chain_window() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: 0,
        summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 60_000,
    }));
    let shift = PhysicalASAPDAGNode {
        id: 1,
        output_state: ExecutionDataState::INGESTION_ROWS,
        lifetime: Lifetime::AcrossEvaluations(StorageMedium::Memory),
        retention_ms: Some(3_900_000),
        ..node(Payload::NonASAP(NonASAPOp::TimeShift {
            shift: asap_types::ir::operator::arguments::TimeShift {
                offset_ms: 3_600_000,
                at: None,
            },
            child: 0,
        }))
    };
    let window_ms = Some(60_000);
    let one_window_of_states = ProducerOutput {
        payload: &chain.payload,
        output_schema: &chain.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 16_000,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Exact(&ExactKind::Sum),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: Some(60_000),
    };
    let usage = node_resource_usage_and_cost(
        &shift,
        &[one_window_of_states],
        (1, 1.0),
        window_ms,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(usage.peak_memory_bytes, 16_000 * 66);
}

/// The bytes of a DDSketch state at α = 0.01 built over `column` of the
/// 600 rows `producer` emits from `source`, which passes on
/// `unchanged_columns` unchanged.
fn ddsketch_state_bytes(
    data: &DataWorkload,
    producer: Payload,
    source: &str,
    column: &str,
    unchanged_columns: BTreeSet<&str>,
) -> u64 {
    use asap_types::ir::schema::{DataType, Field};
    use asap_types::summary_type::SketchAlgorithm;
    let producer = PhysicalASAPDAGNode {
        output_schema: Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain(column, DataType::Float64, false),
        ]),
        ..node(producer)
    };
    let build = PhysicalASAPDAGNode {
        parameters: Some(SketchParams::DDSketch { alpha: 0.01 }),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::Named(column.to_string())),
            reduction: Reduction::Reduce(GroupKeys::default()),
            filter: None,
        }))
    };
    let producer_output = ProducerOutput {
        payload: &producer.payload,
        output_schema: &producer.output_schema,
        output: EdgeStatistics {
            rows: 600,
            bytes: 9_600,
        },
        sources: BTreeSet::from([source]),
        family: StateFamily::None,
        unchanged_columns,
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
    };
    let settings = CostInputs::default();
    node_resource_usage_and_cost(&build, &[producer_output], (1, 1.0), None, data, &settings)
        .unwrap()
        .output
        .bytes
}

/// The bytes `sketch_state_bytes` gives a DDSketch state at α = 0.01 over 600
/// rows of the given sign and magnitude range.
fn ddsketch_costed_bytes(
    non_negative: bool,
    value_magnitude_range: Option<asap_types::workload::ValueMagnitudeRange>,
) -> u64 {
    use crate::node::state_bytes::{sketch_state_bytes, StateInputRows};
    sketch_state_bytes(
        &SketchParams::DDSketch { alpha: 0.01 },
        StateInputRows {
            rows_per_state: Some(600),
            non_negative,
            value_magnitude_range,
        },
    )
}

/// A scan of PromQL metric `metric`.
fn metric_scan(metric: &str) -> Payload {
    Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: Schema::default(),
    })
}

/// A data workload whose one source `source` declares `facts`.
fn declaring(source: &str, facts: asap_types::workload::SourceFacts) -> DataWorkload {
    DataWorkload {
        sources: BTreeMap::from([(source.to_string(), facts)]),
        ..DataWorkload::default()
    }
}

/// A DDSketch built over a scan's `value` is costed at the bins its
/// source's declared `column_magnitudes` span: 807 at α = 0.01 for
/// [0.001, 10000], against 1,037 for the assumed [1, 1e9]. Undeclared, or
/// a column its producer does not pass on unchanged, keeps the assumed
/// range.
#[test]
fn a_ddsketch_over_a_scan_is_costed_at_its_declared_value_magnitude_range() {
    use asap_types::workload::{SourceFacts, ValueMagnitudeRange};
    let latency = ValueMagnitudeRange {
        min_abs: 0.001,
        max_abs: 10_000.0,
    };
    let data = |range: Option<ValueMagnitudeRange>| {
        declaring(
            "latency",
            SourceFacts {
                column_magnitudes: range
                    .map(|range| ("value".to_string(), range))
                    .into_iter()
                    .collect(),
                ..SourceFacts::default()
            },
        )
    };
    let state_bytes = |data: &DataWorkload, unchanged_columns| {
        ddsketch_state_bytes(
            data,
            metric_scan("latency"),
            "latency",
            "value",
            unchanged_columns,
        )
    };
    let unchanged = || BTreeSet::from(["ts", "value"]);
    assert_eq!(
        state_bytes(&data(Some(latency)), unchanged()),
        ddsketch_costed_bytes(false, Some(latency))
    );
    assert_eq!(
        state_bytes(&data(None), unchanged()),
        ddsketch_costed_bytes(false, None)
    );
    assert_eq!(
        state_bytes(&data(Some(latency)), BTreeSet::from(["ts"])),
        ddsketch_costed_bytes(false, None)
    );
    assert!(ddsketch_costed_bytes(false, Some(latency)) < ddsketch_costed_bytes(false, None));
}

/// A DDSketch over a SQL table's `latency` column that the table declares
/// non-negative keeps one store: the sign is read for the folded column,
/// not for a PromQL `value`. Undeclared, it keeps both stores.
#[test]
fn a_ddsketch_over_a_sql_column_declared_non_negative_keeps_one_store() {
    use asap_types::stages::ValueSign;
    use asap_types::workload::SourceFacts;
    let table_scan = || {
        Payload::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "requests".into(),
            },
            predicates: vec![],
            schema: Schema::default(),
        })
    };
    let declared = declaring(
        "requests",
        SourceFacts {
            column_signs: BTreeMap::from([("latency".to_string(), ValueSign::NonNegative)]),
            ..SourceFacts::default()
        },
    );
    let state_bytes = |data: &DataWorkload| {
        ddsketch_state_bytes(
            data,
            table_scan(),
            "requests",
            "latency",
            BTreeSet::from(["ts", "latency"]),
        )
    };
    assert_eq!(state_bytes(&declared), ddsketch_costed_bytes(true, None));
    assert_eq!(
        state_bytes(&DataWorkload::default()),
        ddsketch_costed_bytes(false, None)
    );
    assert!(ddsketch_costed_bytes(true, None) < ddsketch_costed_bytes(false, None));
}

/// A DDSketch over a filter on a scan of a source declared non-negative
/// keeps one store: the filter passes `value` on unchanged. A producer
/// that computes `value` (not among its unchanged columns) keeps both.
#[test]
fn a_ddsketch_over_a_filter_on_a_non_negative_scan_keeps_one_store() {
    use asap_types::ir::scalar::{Predicate, ScalarExpr, ScalarValue};
    use asap_types::stages::ValueSign;
    use asap_types::workload::SourceFacts;
    let filter = || {
        Payload::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            child: 0,
        })
    };
    let data = declaring(
        "latency",
        SourceFacts {
            column_signs: BTreeMap::from([("value".to_string(), ValueSign::NonNegative)]),
            ..SourceFacts::default()
        },
    );
    let state_bytes = |unchanged_columns| {
        ddsketch_state_bytes(&data, filter(), "latency", "value", unchanged_columns)
    };
    assert_eq!(
        state_bytes(BTreeSet::from(["ts", "value"])),
        ddsketch_costed_bytes(true, None)
    );
    assert_eq!(
        state_bytes(BTreeSet::from(["ts"])),
        ddsketch_costed_bytes(false, None)
    );
}

/// A counter's `value` is non-negative unless the source declares another
/// sign for it: a counter `requests_total` declaring `{"value": "unknown"}`
/// keeps both stores, the sign the memo gives its `Scan` too. Without the
/// declaration it keeps one store.
#[test]
fn a_ddsketch_over_a_counter_declaring_its_value_unknown_keeps_both_stores() {
    use asap_types::stages::ValueSign;
    use asap_types::workload::{PrometheusMetricType, SourceFacts};
    let counter = |column_signs| {
        declaring(
            "requests_total",
            SourceFacts {
                metric_type: Some(PrometheusMetricType::Counter),
                column_signs,
                ..SourceFacts::default()
            },
        )
    };
    let state_bytes = |data: &DataWorkload| {
        ddsketch_state_bytes(
            data,
            metric_scan("requests_total"),
            "requests_total",
            "value",
            BTreeSet::from(["ts", "value"]),
        )
    };
    assert_eq!(
        state_bytes(&counter(BTreeMap::from([(
            "value".to_string(),
            ValueSign::Unknown
        )]))),
        ddsketch_costed_bytes(false, None)
    );
    assert_eq!(
        state_bytes(&counter(BTreeMap::new())),
        ddsketch_costed_bytes(true, None)
    );
}
