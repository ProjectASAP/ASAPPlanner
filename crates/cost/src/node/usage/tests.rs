//! Tests of [`super::node_resource_usage_and_cost`].

use std::time::Duration;

use asap_types::ir::properties::ExecutionDataState;
use asap_types::ir::scalar::ColumnRef;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::plan::time_window::TimeIndexedRead;
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
/// `TimeRange`, or above the `TimeRange`, changes nothing. The range
/// selects five windows of 1000 states, and the merge over them reads
/// 5000 states into 1000, as with no shift (it read 1000, one window).
#[test]
fn a_time_shift_over_a_chain_or_its_windows_selects_what_the_range_does() {
    use asap_types::ir::operator::TimeShift;
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
    for path in [[&shift, &range], [&range, &shift]] {
        assert_eq!(merge_over(&path), direct);
    }
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
    let read_ms = read_back_ms(
        0,
        |id| match id {
            0 => vec![1],
            _ => vec![],
        },
        |_| TimeIndexedRead::of(&instant.payload, ExecutionTiming::QueryTime),
        |_| 0,
        true,
    );
    assert_eq!(read_ms, Some(1_000));
    let usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), read_ms, &data, &settings).unwrap();
    assert_eq!(usage.output.rows, 1_000);
    let (_, kept_bytes) = usage.raw_scan.expect("a query-time scan keeps raw data");
    assert_eq!(kept_bytes, usage.output.bytes * 3_601);
}
