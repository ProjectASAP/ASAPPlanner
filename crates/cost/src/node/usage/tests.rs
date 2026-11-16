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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
            raw_rows_span_ms: None,
            state_input_rows: None,
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
            + crate::node::operations_per_unit::merge_output_operations(merge_usage.family, 1_000)
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
            + crate::node::operations_per_unit::merge_output_operations(merge_usage.family, 1_000)
    );
}

/// 1,000 series each of `latency` (a non-negative gauge) and
/// `requests_total` (a counter), sampled every `sample_interval_ms`.
fn thousand_series_sampled_every(sample_interval_ms: u64) -> DataWorkload {
    serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0 / sample_interval_ms as f64}},
        "sources": {
            "latency": {"series_count": 1000, "metric_type": "gauge",
                "column_signs": {"value": "non_negative"}, "sample_interval": sample_interval_ms},
            "requests_total": {"series_count": 1000, "metric_type": "counter",
                "sample_interval": sample_interval_ms}
        }
    }))
    .unwrap()
}

/// `rows` raw rows of `metric` spanning `span_ms`, as `producer` outputs
/// them to a grouping.
fn raw_rows<'a>(
    producer: &'a PhysicalASAPDAGNode,
    metric: &'static str,
    rows: u64,
    span_ms: u64,
) -> ProducerOutput<'a> {
    ProducerOutput {
        payload: &producer.payload,
        output_schema: &producer.output_schema,
        output: EdgeStatistics {
            rows,
            bytes: rows * 48,
        },
        sources: BTreeSet::from([metric]),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: Some(span_ms),
        state_input_rows: None,
    }
}

/// The output schema of a per-series grouping that carries `job` beside
/// the series identity (its key), with a `state` column of `state_type`
/// when it builds states.
fn per_series_carrying_job(state_type: Option<FieldDataType>) -> Schema {
    use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
    let mut fields = vec![Field::new(
        "ts",
        FieldDataType::Plain(DataType::Timestamp),
        false,
    )];
    fields.extend(state_type.map(|state_type| Field::new("state", state_type, false)));
    fields.extend(
        ["job", PROMQL_SERIES_IDENTITY]
            .map(|label| Field::new(label, FieldDataType::Plain(DataType::Utf8), false)),
    );
    Schema::new(fields)
}

/// A per-series Rate chain carrying `job` in 300 s windows at ingestion
/// time over 1,000 series of `requests_total`: measured 106.3 ns per
/// arriving row at 1 Hz and 87.6 at 10 Hz (a stored-only plan's ingestion
/// CPU over its rows), 1.21x; an earlier run measured 125.6 and 85.7, as 1 Hz
/// ingestion varies between runs. Its per-row work is the same at both
/// rates; what differs is how often it visits each series' state, once per
/// 10 s raw batch, which holds 10 rows of a series at 1 Hz and 100 at
/// 10 Hz. Charged a time range per row instead, the two rates differed by
/// 5%.
#[test]
fn an_ingestion_time_chain_costs_more_per_row_at_a_lower_sample_rate() {
    let settings = CostInputs::default();
    let rate = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    let scan = node(metric_scan("requests_total"));
    let chain = PhysicalASAPDAGNode {
        retention_ms: Some(300_000),
        output_state: ExecutionDataState::INGESTION_SUMMARY,
        output_schema: per_series_carrying_job(Some(rate.clone())),
        ..node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: rate.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 300_000,
        }))
    };
    // One second of arriving rows per execution, one execution per second.
    let ns_per_row = |sample_interval_ms: u64| {
        let rows_per_second = 1_000 * 1_000 / sample_interval_ms;
        let usage = node_resource_usage_and_cost(
            &chain,
            &[raw_rows(&scan, "requests_total", rows_per_second, 1_000)],
            (1, 1.0),
            None,
            &thousand_series_sampled_every(sample_interval_ms),
            &settings,
        )
        .unwrap();
        usage.cpu_ops_per_second / rows_per_second as f64
    };
    let (one_hz, ten_hz) = (ns_per_row(1_000), ns_per_row(100));
    for (charged, measured) in [(one_hz, 106.3), (ten_hz, 87.6)] {
        assert!(
            (0.8..=1.25).contains(&(charged / measured)),
            "charged {charged:.1} ns per row, measured {measured}"
        );
    }
    assert!(
        (1.1..=1.4).contains(&(one_hz / ten_hz)),
        "1 Hz charged {one_hz:.1} ns per row, 10 Hz {ten_hz:.1}; measured 1.21x"
    );
}

/// A per-series `max_over_time(latency[5m])` hash aggregate at query time
/// over 1,000 series measures 40.5 ns per row at 1 Hz (300,000 rows) and
/// 20.9 at 10 Hz (3,000,000 rows), its output rows included. The executor
/// visits each series' group once per 10 s raw batch, 10 rows of it at
/// 1 Hz and 100 at 10 Hz. Charged per row alone, the 10 Hz aggregate was
/// 1.33x over-charged, and at 1 Hz an exact scan looked cheaper than the
/// stored plan it measured 1.24x more than.
#[test]
fn a_per_series_aggregate_costs_more_per_row_at_a_lower_sample_rate() {
    use asap_types::ir::operator::AggIntent;
    let settings = CostInputs::default();
    let range = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let aggregate = PhysicalASAPDAGNode {
        output_schema: {
            use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
            Schema::new(vec![
                Field::new("value", FieldDataType::Plain(DataType::Float64), false),
                Field::new(
                    PROMQL_SERIES_IDENTITY,
                    FieldDataType::Plain(DataType::Utf8),
                    false,
                ),
            ])
        },
        ..node(Payload::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Max { col: None }],
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
            child: 0,
        }))
    };
    let ns_per_row = |sample_interval_ms: u64| {
        let rows = 300 * 1_000 * 1_000 / sample_interval_ms;
        let usage = node_resource_usage_and_cost(
            &aggregate,
            &[raw_rows(&range, "latency", rows, 300_000)],
            (1, 1.0),
            None,
            &thousand_series_sampled_every(sample_interval_ms),
            &settings,
        )
        .unwrap();
        usage.work_ms_per_execution * 1e6 / rows as f64
    };
    for (sample_interval_ms, measured) in [(1_000, 40.5), (100, 20.9)] {
        let charged = ns_per_row(sample_interval_ms);
        assert!(
            (0.85..=1.18).contains(&(charged / measured)),
            "every {sample_interval_ms} ms: charged {charged:.1} ns per row, measured {measured}"
        );
    }
}

/// A grouping visits each group's state once per raw batch of the
/// deployment's `raw_batch_width_ms`: the per-series aggregate over 5 min
/// of 1,000 series at 1 Hz visits each series 30 times in 10 s batches and
/// 300 times in 1 s batches (one row each), 270,000 visits × 170
/// operations more.
#[test]
fn a_grouping_visits_each_group_once_per_raw_batch_of_the_declared_width() {
    use crate::node::operations_per_unit::OPERATIONS_PER_GROUP_PER_RAW_BATCH;
    use asap_types::ir::operator::AggIntent;
    let range = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![AggIntent::Max { col: None }],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: 0,
    }));
    let cpu_ops = |raw_batch_width_ms: u64| {
        let mut settings = CostInputs::default();
        settings.resource_speed.raw_batch_width_ms = raw_batch_width_ms;
        node_resource_usage_and_cost(
            &aggregate,
            &[raw_rows(&range, "latency", 300_000, 300_000)],
            (1, 1.0),
            None,
            &thousand_series_sampled_every(1_000),
            &settings,
        )
        .unwrap()
        .cpu_ops_per_second
    };
    let extra = cpu_ops(1_000) - cpu_ops(10_000);
    assert!(
        (extra - 270_000.0 * OPERATIONS_PER_GROUP_PER_RAW_BATCH).abs() < 1.0,
        "1 s batches cost {extra} operations more"
    );
}

/// A per-series exact Rate carrying `job`, built at query time from 5 min
/// of 1,000 series of `requests_total`, measures 85.9 ns per row at 1 Hz
/// and 68.8 at 10 Hz (1.25x): its update is the same per row, and it visits
/// each series' state once per 10 s raw batch. Charged per row alone, both
/// were charged the same.
#[test]
fn a_query_time_rate_build_costs_more_per_row_at_a_lower_sample_rate() {
    let settings = CostInputs::default();
    let rate = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    let range = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let build = PhysicalASAPDAGNode {
        output_schema: per_series_carrying_job(Some(rate.clone())),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: rate.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        }))
    };
    let ns_per_row = |sample_interval_ms: u64| {
        let rows = 300 * 1_000 * 1_000 / sample_interval_ms;
        let usage = node_resource_usage_and_cost(
            &build,
            &[raw_rows(&range, "requests_total", rows, 300_000)],
            (1, 1.0),
            None,
            &thousand_series_sampled_every(sample_interval_ms),
            &settings,
        )
        .unwrap();
        usage.work_ms_per_execution * 1e6 / rows as f64
    };
    let (one_hz, ten_hz) = (ns_per_row(1_000), ns_per_row(100));
    for (charged, measured) in [(one_hz, 85.9), (ten_hz, 68.8)] {
        assert!(
            (0.8..=1.25).contains(&(charged / measured)),
            "charged {charged:.1} ns per row, measured {measured}"
        );
    }
    assert!(
        (1.1..=1.4).contains(&(one_hz / ten_hz)),
        "1 Hz charged {one_hz:.1} ns per row, 10 Hz {ten_hz:.1}; measured 1.25x"
    );
}

/// A query-time chain reads its scan's lookback and keeps each row in its
/// window's range, as a `TimeRange` over the same rows does, besides
/// updating the row's state and visiting its group once per raw batch.
/// Example: a sum in 60 s windows over 36,000 raw rows of unknown rate (one
/// batch), retaining five windows of one group, costs 36,000 × 40
/// operations (29 to update, 11 to keep in range), one group visit × 170
/// and five emitted exact states × 1,500.
#[test]
fn a_query_time_chain_pays_the_time_range_over_the_rows_it_reads() {
    use crate::node::operations_per_unit::{
        EXACT_UPDATE_OPERATIONS_PER_ROW, OPERATIONS_PER_GROUP_PER_RAW_BATCH,
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
            + OPERATIONS_PER_GROUP_PER_RAW_BATCH
            + 5.0 * 1_500.0
    );
}

/// A query-time scan of 5 min of `latency` (1,000 series at 1 Hz, 300,000
/// rows) under a filter that keeps a tenth of each series' rows, e.g.
/// `latency > 100`: the 30,000 rows left still span 5 min, 30 raw batches
/// of 10 s, so a per-series grouping visits each of its 1,000 groups 30
/// times, at most once per row: 30,000 visits. Counted from its input rows
/// over the sources' rate, the rows spanned 30 s, 3 batches, 3,000 visits.
/// A `Filter` passes its input's span on, as a `TimeRange` and the
/// operators that keep each row's window do.
#[test]
fn a_grouping_counts_raw_batches_from_the_time_its_input_spans() {
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::scalar::{Predicate, ScalarExpr, ScalarValue};
    let settings = CostInputs::default();
    let data = thousand_series_sampled_every(1_000);
    let scan = node(metric_scan("latency"));
    let scan_usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), Some(300_000), &data, &settings)
            .unwrap();
    assert_eq!(scan_usage.output.rows, 300_000);
    let filter = node(Payload::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        child: 0,
    }));
    let filter_usage = node_resource_usage_and_cost(
        &filter,
        &[produced(&scan, &scan_usage)],
        (1, 1.0),
        None,
        &data,
        &settings,
    )
    .unwrap();
    // The filter keeps a tenth of the rows; the cost model passes every
    // row on, so the test sets what it keeps.
    let mut kept = produced(&filter, &filter_usage);
    kept.output.rows = 30_000;
    kept.output.bytes /= 10;
    let aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![AggIntent::Max { col: None }],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: 0,
    }));
    let usage = node_resource_usage_and_cost(&aggregate, &[kept], (1, 1.0), None, &data, &settings)
        .unwrap();
    assert!(
        usage.cost.detail.contains(", 30000 group visits"),
        "{}",
        usage.cost.detail
    );
}

/// A sort, a limit and a dedup pass raw rows on as they are, so they pass
/// on the time those rows span, as a `Filter` does; a concat of raw rows
/// spans the longest of its inputs'. Example: a 5 min scan of 1,000 series
/// at 1 Hz sorted spans 300,000 ms, 30 batches of 10 s, so a per-series
/// grouping over it visits each group 30 times: 30,000 visits, where
/// without a span it counted one batch, 1,000.
#[test]
fn a_sort_limit_dedup_and_concat_pass_on_the_time_their_raw_rows_span() {
    use asap_types::ir::operator::GroupKeys;
    use asap_types::ir::scalar::{ScalarExpr, ScalarValue, SortKey};
    let settings = CostInputs::default();
    let data = thousand_series_sampled_every(1_000);
    let scan = node(metric_scan("latency"));
    let scan_usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), Some(300_000), &data, &settings)
            .unwrap();
    let short_scan_usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), Some(60_000), &data, &settings).unwrap();
    let span_of = |payload: Payload, producers: Vec<ProducerOutput<'_>>| {
        node_resource_usage_and_cost(&node(payload), &producers, (1, 1.0), None, &data, &settings)
            .unwrap()
            .raw_rows_span_ms
    };
    let raw = || produced(&scan, &scan_usage);
    for payload in [
        Payload::NonASAP(NonASAPOp::Sort {
            keys: vec![SortKey {
                expr: ScalarExpr::Literal(ScalarValue::Float64(1.0)),
                ascending: true,
                nulls_first: false,
            }],
            partition_by: GroupKeys::default(),
            child: 0,
        }),
        Payload::NonASAP(NonASAPOp::Limit {
            n: Some(10),
            offset: 0,
            partition_by: GroupKeys::default(),
            child: 0,
        }),
        Payload::NonASAP(NonASAPOp::Dedup {
            cols: vec![],
            child: 0,
        }),
    ] {
        assert_eq!(
            span_of(payload.clone(), vec![raw()]),
            Some(300_000),
            "{payload:?}"
        );
    }
    let concat = || {
        Payload::NonASAP(NonASAPOp::Concat {
            children: vec![0, 1],
        })
    };
    assert_eq!(
        span_of(concat(), vec![raw(), produced(&scan, &short_scan_usage)]),
        Some(300_000)
    );
    // A concat with an input that is not raw rows outputs no raw rows.
    let aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![asap_types::ir::operator::AggIntent::Max { col: None }],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: 0,
    }));
    let aggregate_usage =
        node_resource_usage_and_cost(&aggregate, &[raw()], (1, 1.0), None, &data, &settings)
            .unwrap();
    assert_eq!(
        span_of(
            concat(),
            vec![raw(), produced(&aggregate, &aggregate_usage)]
        ),
        None
    );
}

/// A query-time chain of 60 s windows over the same filtered scan (30,000
/// rows over 5 min): each window holds the rows of its 60 s, 6,000, so
/// each of its 1,000 per-series states folds 6 rows. Sized by the sources'
/// rate, a window held 60,000 rows (all 30,000 read), 30 per state.
#[test]
fn a_query_time_chain_sizes_its_windows_from_the_time_its_input_spans() {
    let settings = CostInputs::default();
    let data = thousand_series_sampled_every(1_000);
    let scan = node(metric_scan("latency"));
    let scan_usage =
        node_resource_usage_and_cost(&scan, &[], (1, 1.0), Some(300_000), &data, &settings)
            .unwrap();
    let mut kept = produced(&scan, &scan_usage);
    kept.output.rows = 30_000;
    kept.output.bytes /= 10;
    let chain = PhysicalASAPDAGNode {
        retention_ms: Some(300_000),
        ..node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::Sketch(asap_types::summary_type::SketchAlgorithm::Kll),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }))
    };
    let chain = PhysicalASAPDAGNode {
        parameters: Some(SketchParams::Kll { k: 547 }),
        ..chain
    };
    let usage =
        node_resource_usage_and_cost(&chain, &[kept], (1, 1.0), None, &data, &settings).unwrap();
    assert_eq!(
        usage.state_input_rows.and_then(|rows| rows.rows_per_state),
        Some(6)
    );
}

/// A merge that reads one state per group passes each state through: the
/// executor moves a group's first state into its slot without merging it
/// (`merge_objects`), so it pays only each group's row. Merging the one
/// 10 s window a time range selects from a KLL chain (k = 547, 1,000
/// series at 10 Hz, 100 rows per state) measures 1.0 ms, and from a
/// DDSketch chain 0.97 ms; charged as merges, they were 29.3 and 3.45 ms.
#[test]
fn a_merge_reading_one_state_per_group_passes_it_through() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let window = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(10),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    for (parameters, measured_ms) in [
        (SketchParams::Kll { k: 547 }, 1.0),
        (SketchParams::DDSketch { alpha: 0.01 }, 0.97),
    ] {
        let state_bytes = crate::node::state_bytes::sketch_state_bytes(
            &parameters,
            StateInputRows {
                rows_per_state: Some(100),
                non_negative: true,
                value_magnitude_range: None,
                distinct_values: None,
            },
        );
        let one_window = ProducerOutput {
            payload: &window.payload,
            output_schema: &window.output_schema,
            output: EdgeStatistics {
                rows: 1_000,
                bytes: 1_000 * state_bytes,
            },
            sources: BTreeSet::new(),
            family: StateFamily::None,
            unchanged_columns: BTreeSet::new(),
            tumbling_windows: 1,
            chain_tumbling_window_width_ms: None,
            raw_rows_span_ms: None,
            state_input_rows: Some(StateInputRows {
                rows_per_state: Some(100),
                non_negative: true,
                value_magnitude_range: None,
                distinct_values: None,
            }),
        };
        let merge = PhysicalASAPDAGNode {
            parameters: Some(parameters.clone()),
            ..node(Payload::ASAP(ASAPOp::SummaryMerge {
                children: vec![0],
                group_by: None,
            }))
        };
        let usage =
            node_resource_usage_and_cost(&merge, &[one_window], (1, 1.0), None, &data, &settings)
                .unwrap();
        let charged_ms = usage.work_ms_per_execution;
        assert!(
            (0.8..=1.25).contains(&(measured_ms / charged_ms)),
            "{parameters:?}: charged {charged_ms:.3} ms, measured {measured_ms}"
        );
    }
}

/// A SQL table `t` of 1,000 groups of rows (`instance`) sampled every
/// second, 10 `job`s and 4 `region`s, whose `user_id` takes
/// `user_ids` distinct values; 300,000 of its rows, as a time range
/// passes them to a grouping.
fn sql_rows_of_t(user_ids: u64) -> (DataWorkload, Schema) {
    use asap_types::ir::schema::DataType;
    let data = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 1000.0}},
        "sources": {"t": {"series_count": 1000, "sample_interval": 1000,
            "distinct_values_per_label": {"job": 10, "region": 4, "instance": 1000,
                "user_id": user_ids}}}
    }))
    .unwrap();
    let schema = Schema::new(vec![
        Field::new("ts", FieldDataType::Plain(DataType::Timestamp), false),
        Field::new("job", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("region", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("instance", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("user_id", FieldDataType::Plain(DataType::Float64), false),
        Field::new("value", FieldDataType::Plain(DataType::Float64), false),
    ]);
    (data, schema)
}

/// The CPU operations per input row a query-time hash aggregate of
/// `measures` grouped by `keys` charges over [`sql_rows_of_t`]'s 300,000
/// rows.
fn sql_aggregate_operations_per_row(
    keys: Vec<usize>,
    measures: Vec<asap_types::ir::operator::AggIntent>,
    user_ids: u64,
) -> f64 {
    let settings = CostInputs::default();
    let (data, schema) = sql_rows_of_t(user_ids);
    let range = PhysicalASAPDAGNode {
        output_schema: schema,
        ..node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        }))
    };
    let mut rows = raw_rows(&range, "t", 300_000, 300_000);
    rows.output_schema = &range.output_schema;
    rows.unchanged_columns = ["job", "region", "instance", "user_id", "value"].into();
    let names = measures.iter().map(|_| String::new()).collect();
    let aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(keys),
        measures,
        output_names: names,
        filters: vec![],
        having: None,
        child: 0,
    }));
    let usage = node_resource_usage_and_cost(&aggregate, &[rows], (1, 1.0), None, &data, &settings)
        .unwrap();
    usage.cpu_ops_per_second / 300_000.0
}

/// A SQL hash aggregate pays per row for each grouping key beyond the
/// first and for each measure, as measured on the executor (300,000 rows
/// of 1,000 groups, 100 rows per group per raw batch, so the group visits
/// are negligible): `GROUP BY job, instance` costs 34.0 ns per row more
/// than `GROUP BY instance`, `GROUP BY job, region, instance` 50.4 more,
/// and `MAX, MIN, SUM, COUNT` 11.9 more than `MAX` alone. Charged 15 per
/// key and 3 per measure, the second and third keys were 0.44x and 0.60x
/// their measured cost.
#[test]
fn a_hash_aggregate_pays_each_further_key_and_measure_as_measured() {
    use asap_types::ir::operator::AggIntent;
    let max = || vec![AggIntent::Max { col: Some(5) }];
    let four = || {
        vec![
            AggIntent::Max { col: Some(5) },
            AggIntent::Min { col: Some(5) },
            AggIntent::Sum { col: Some(5) },
            AggIntent::Count,
        ]
    };
    let one_key = sql_aggregate_operations_per_row(vec![3], max(), 1);
    for (charged, measured, what) in [
        (
            sql_aggregate_operations_per_row(vec![1, 3], max(), 1) - one_key,
            34.0,
            "a second key",
        ),
        (
            sql_aggregate_operations_per_row(vec![1, 2, 3], max(), 1) - one_key,
            50.4,
            "two more keys",
        ),
        (
            sql_aggregate_operations_per_row(vec![3], four(), 1) - one_key,
            11.9,
            "three more measures",
        ),
    ] {
        assert!(
            (0.8..=1.25).contains(&(charged / measured)),
            "{what}: charged {charged:.1} ns per row more, measured {measured}"
        );
    }
}

/// An exact `COUNT(DISTINCT user_id)` over 300,000 rows interns each value
/// in a table of every distinct value read, then keeps each group's
/// (group, value) pairs: its time per row grows with the distinct values.
/// Measured on the executor with one group: 115 ns per row over 10,000
/// distinct users and 589 over 1,000,000 (259,000 of them among the rows);
/// charged 18 at both.
#[test]
fn an_exact_distinct_count_costs_more_per_row_with_more_distinct_values() {
    use asap_types::ir::operator::AggIntent;
    for (user_ids, measured) in [(10_000, 115.0), (1_000_000, 589.0)] {
        let charged = sql_aggregate_operations_per_row(
            vec![],
            vec![AggIntent::Cardinality { cols: vec![4] }],
            user_ids,
        );
        assert!(
            (0.75..=1.33).contains(&(charged / measured)),
            "{user_ids} distinct: charged {charged:.1} ns per row, measured {measured}"
        );
    }
}

/// A grouped exact `COUNT(DISTINCT user_id)` pays its grouping key and its
/// group visits once: its own terms were fitted with them fixed at their
/// constants. Measured on the executor over 300,000 rows by `job` (10
/// groups): 60.2 ns per row of 100 distinct users and 81.3 of 1,000. With
/// the key and the visits also inside the count's fitted constant, they
/// were charged 83 and 97, 1.38x and 1.19x. The count's terms are fitted
/// to within 0.76–1.32x of 119 measured counts, so the bound is that
/// range: they are charged 80 and 82. The bound is wider than 0.8–1.25
/// because the 100-user point is noisy: over the same 300,000 rows of 100
/// users, the count by `job` takes 60.2 ns per row and the one by `region`
/// (4 groups) 86.1, 1.43x apart at the same values, and the four grouped counts
/// over 100 values span 60–92. Every count under 1,000 values is charged
/// as at 1,000, so the 60.2 point is charged 1.33x, near the bound.
#[test]
fn a_grouped_exact_distinct_count_pays_its_key_and_visits_once() {
    use asap_types::ir::operator::AggIntent;
    for (user_ids, measured) in [(100, 60.2), (1_000, 81.3)] {
        let charged = sql_aggregate_operations_per_row(
            vec![1],
            vec![AggIntent::Cardinality { cols: vec![4] }],
            user_ids,
        );
        assert!(
            (0.75..=1.35).contains(&(charged / measured)),
            "by job of {user_ids} users: charged {charged:.1} ns per row, measured {measured}"
        );
    }
}

/// The CPU operations per input row a query-time exact
/// `COUNT(DISTINCT user_id)` grouped by `keys` charges over `rows` rows of
/// `http` (10,000 series at one row per second each, so the rows span
/// `rows` ÷ 10,000 s; `region` takes 10 values, `service` 100 and
/// `instance` 10,000). `user_ids` declares the distinct `user_id`s and
/// `max_distinct_per_evaluation` the source's bound on each group's distinct
/// count; either may be absent.
fn http_distinct_count_operations_per_row(
    keys: Vec<usize>,
    rows: u64,
    user_ids: Option<u64>,
    max_distinct_per_evaluation: Option<u64>,
) -> f64 {
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::schema::DataType;
    let settings = CostInputs::default();
    let mut labels = serde_json::json!({"region": 10, "service": 100, "instance": 10_000});
    if let Some(user_ids) = user_ids {
        labels["user_id"] = user_ids.into();
    }
    let mut source = serde_json::json!({"series_count": 10_000, "sample_interval": 1000,
        "distinct_values_per_label": labels});
    if let Some(bound) = max_distinct_per_evaluation {
        source["max_distinct_per_evaluation"] = bound.into();
    }
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 10_000.0}},
        "sources": {"http": source}
    }))
    .unwrap();
    let schema = Schema::new(
        [
            ("ts", DataType::Timestamp),
            ("region", DataType::Utf8),
            ("service", DataType::Utf8),
            ("instance", DataType::Utf8),
            ("user_id", DataType::Float64),
        ]
        .map(|(name, dtype)| Field::new(name, FieldDataType::Plain(dtype), false))
        .to_vec(),
    );
    let range = PhysicalASAPDAGNode {
        output_schema: schema,
        ..node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(rows / 10_000),
            kind: TimeRangeKind::Range,
            child: 0,
        }))
    };
    let mut input = raw_rows(&range, "http", rows, rows / 10);
    input.output_schema = &range.output_schema;
    input.unchanged_columns = ["region", "service", "instance", "user_id"].into();
    let aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(keys),
        measures: vec![AggIntent::Cardinality { cols: vec![4] }],
        output_names: vec![String::new()],
        filters: vec![],
        having: None,
        child: 0,
    }));
    let usage =
        node_resource_usage_and_cost(&aggregate, &[input], (1, 1.0), None, &data, &settings)
            .unwrap();
    usage.cpu_ops_per_second / rows as f64
}

/// An exact `COUNT(DISTINCT user_id)` looks each row's value up in a table
/// of every distinct value read, then its (group, value) pair in a set of
/// every pair kept: a lookup misses the cache once its table outgrows it,
/// so the time per row follows the values and the pairs, not the rows.
/// Measured on the executor over 12,000,000 rows of 10,000 series (10 rows
/// per series per batch, median of 3 runs on one pinned core): 1,000 users
/// by `instance` (10,000 groups, 7.0 M pairs) take 179 ns per row; 10,000
/// ungrouped 106; 1,000,000 ungrouped 381, by `service` 488 and 10,000,000
/// by (`region`, `service`) 811. Charged per square root of the values and
/// per inserted entry, they were 139, 99, 587, 665 and 1,141: 0.78x to
/// 1.54x.
#[test]
fn an_exact_distinct_count_costs_per_value_and_pair_it_looks_up() {
    for (keys, user_ids, measured) in [
        (vec![3], 1_000, 179.1),
        (vec![], 10_000, 106.2),
        (vec![], 1_000_000, 381.1),
        (vec![2], 1_000_000, 488.4),
        (vec![1, 2], 10_000_000, 811.3),
    ] {
        let charged =
            http_distinct_count_operations_per_row(keys.clone(), 12_000_000, Some(user_ids), None);
        assert!(
            (0.8..=1.25).contains(&(charged / measured)),
            "by {keys:?} of {user_ids} users: charged {charged:.1} ns per row, measured {measured}"
        );
    }
}

/// `max_distinct_per_evaluation` bounds the distinct items of each group,
/// so a grouped count's table of every distinct value may hold that bound
/// times the groups: 10,000 `instance`s of 1,000 users each are 10,000,000
/// users. A workload whose users are shared across the groups declares the
/// column-wide `distinct_values_per_label`, which caps the table instead.
/// `COUNT(DISTINCT user_id)` by `instance` over 120,000,000 rows of
/// 1,000,000 users at most, with the per-group bound also 1,000,000, is
/// charged as over the 1,000,000 declared values; with the per-group bound
/// alone, as over the rows read (1,000,000 × 10,000 is more).
#[test]
fn an_exact_distinct_count_reads_at_most_its_column_s_declared_distinct_values() {
    let declared_and_bounded = http_distinct_count_operations_per_row(
        vec![3],
        120_000_000,
        Some(1_000_000),
        Some(1_000_000),
    );
    let declared =
        http_distinct_count_operations_per_row(vec![3], 120_000_000, Some(1_000_000), None);
    let bounded =
        http_distinct_count_operations_per_row(vec![3], 120_000_000, None, Some(1_000_000));
    assert!(
        (declared_and_bounded - declared).abs() < 1e-9 * declared,
        "declared and bounded {declared_and_bounded:.1} ns per row, declared {declared:.1}"
    );
    assert!(
        bounded > declared,
        "bounded per group {bounded:.1} ns per row, declared {declared:.1}"
    );
}

/// A KMV state compares each row's hash with the hashes it retains until
/// it holds `k` of them, then only hashes below its largest
/// (`asap_sketchlib`'s duplicate check). Over 300,000 rows of 100,000
/// distinct `user_id`s, one state at k = 4,323 fills early and measures
/// 191 ns per row; four states by `region` at k = 26,656 (75,000 rows
/// each) stay below `k` for most rows and measure 7,410. Charged a fixed
/// 48 per row, the grouped build looked cheaper than an exact distinct
/// count it measures 25x more than.
#[test]
fn a_kmv_build_costs_per_retained_hash_it_compares() {
    use asap_types::summary_type::SketchAlgorithm;
    let settings = CostInputs::default();
    let (data, schema) = sql_rows_of_t(100_000);
    let range = PhysicalASAPDAGNode {
        output_schema: schema,
        ..node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        }))
    };
    for (keys, k, measured) in [(vec![], 4_323, 191.0), (vec![2], 26_656, 7_410.0)] {
        let mut rows = raw_rows(&range, "t", 300_000, 300_000);
        rows.output_schema = &range.output_schema;
        rows.unchanged_columns = ["job", "region", "instance", "user_id", "value"].into();
        let build = PhysicalASAPDAGNode {
            parameters: Some(SketchParams::Kmv { k }),
            ..node(Payload::ASAP(ASAPOp::SummaryAgg {
                child: 0,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::Kmv),
                input: SummaryUpdate::column(ColumnRef::Named("user_id".into())),
                reduction: Reduction::by(keys.clone()),
                filter: None,
            }))
        };
        let usage = node_resource_usage_and_cost(&build, &[rows], (1, 1.0), None, &data, &settings)
            .unwrap();
        let charged = usage.cpu_ops_per_second / 300_000.0;
        assert!(
            (0.75..=1.33).contains(&(charged / measured)),
            "k = {k}, keys {keys:?}: charged {charged:.1} ns per row, measured {measured}"
        );
    }
}

/// A rollup merge of KMV states updates each group's state with every hash
/// each input state retains (`asap_sketchlib`'s merge), so it compares
/// those hashes with the ones the group's state already retains. Measured
/// on the executor: 1,000 states by (region, service) at k = 106,263, each
/// built from 12,000 rows of 1,000,000 distinct users, merged into 100
/// groups by service take 352 s (the plan's 393 s less its 40 s build).
/// Charged per byte of state, they were 0.1 s.
#[test]
fn a_kmv_rollup_merge_costs_per_hash_it_inserts() {
    use asap_types::ir::schema::DataType;
    use asap_types::summary_type::SketchAlgorithm;
    let settings = CostInputs::default();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0}},
        "sources": {"http": {"distinct_values_per_label": {"service": 100, "region": 10}}}
    }))
    .unwrap();
    let parameters = SketchParams::Kmv { k: 106_263 };
    let rows = StateInputRows {
        rows_per_state: Some(12_000),
        non_negative: false,
        value_magnitude_range: None,
        distinct_values: None,
    };
    let state_bytes = crate::node::state_bytes::sketch_state_bytes(&parameters, rows);
    let build = PhysicalASAPDAGNode {
        parameters: Some(parameters.clone()),
        output_schema: Schema::new(vec![
            Field::new("region", FieldDataType::Plain(DataType::Utf8), false),
            Field::new("service", FieldDataType::Plain(DataType::Utf8), false),
            Field::new("state", FieldDataType::Sketch(SketchAlgorithm::Kmv), false),
        ]),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kmv),
            input: SummaryUpdate::column(ColumnRef::Named("user_id".into())),
            reduction: Reduction::by(vec![0, 1]),
            filter: None,
        }))
    };
    let states = ProducerOutput {
        payload: &build.payload,
        output_schema: &build.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 1_000 * (state_bytes + 32),
        },
        sources: BTreeSet::from(["http"]),
        family: StateFamily::Sketch(&parameters),
        unchanged_columns: ["region", "service"].into(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: Some(rows),
    };
    let rollup = PhysicalASAPDAGNode {
        parameters: Some(parameters.clone()),
        ..node(Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: Some(vec![1]),
        }))
    };
    let usage =
        node_resource_usage_and_cost(&rollup, &[states], (1, 1.0), None, &data, &settings).unwrap();
    assert_eq!(usage.output.rows, 100);
    let charged_s = usage.work_ms_per_execution / 1_000.0;
    assert!(
        (0.75..=1.33).contains(&(352.0 / charged_s)),
        "charged {charged_s:.1} s, measured 352"
    );
}

/// A rollup merges every state its keys put in one group, even when the
/// estimates give it as many groups as states: with no declared label
/// values, KMV states by (region, service) and their rollup by `service`
/// both estimate 1,000 groups, yet each service's states merge into one.
/// Only a merge that keeps its input's groups (no `group_by`, or the
/// input's own keys) passes one state per group through.
#[test]
fn a_rollup_merges_its_states_when_its_group_estimate_ties_its_input() {
    use asap_types::ir::schema::DataType;
    use asap_types::summary_type::SketchAlgorithm;
    let settings = CostInputs::default();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0}},
        "sources": {"http": {}}
    }))
    .unwrap();
    let parameters = SketchParams::Kmv { k: 26_656 };
    let rows = StateInputRows {
        rows_per_state: Some(12_000),
        non_negative: false,
        value_magnitude_range: None,
        distinct_values: None,
    };
    let state_bytes = crate::node::state_bytes::sketch_state_bytes(&parameters, rows);
    let build = PhysicalASAPDAGNode {
        parameters: Some(parameters.clone()),
        output_schema: Schema::new(vec![
            Field::new("region", FieldDataType::Plain(DataType::Utf8), false),
            Field::new("service", FieldDataType::Plain(DataType::Utf8), false),
            Field::new("state", FieldDataType::Sketch(SketchAlgorithm::Kmv), false),
        ]),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kmv),
            input: SummaryUpdate::column(ColumnRef::Named("user_id".into())),
            reduction: Reduction::by(vec![0, 1]),
            filter: None,
        }))
    };
    let states = ProducerOutput {
        payload: &build.payload,
        output_schema: &build.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 1_000 * (state_bytes + 32),
        },
        sources: BTreeSet::from(["http"]),
        family: StateFamily::Sketch(&parameters),
        unchanged_columns: ["region", "service"].into(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: Some(rows),
    };
    let merge_ms = |group_by: Option<Vec<usize>>| {
        let merge = PhysicalASAPDAGNode {
            parameters: Some(parameters.clone()),
            ..node(Payload::ASAP(ASAPOp::SummaryMerge {
                children: vec![0],
                group_by,
            }))
        };
        let usage = node_resource_usage_and_cost(
            &merge,
            std::slice::from_ref(&states),
            (1, 1.0),
            None,
            &data,
            &settings,
        )
        .unwrap();
        assert_eq!(usage.output.rows, 1_000, "{}", usage.cost.detail);
        usage.work_ms_per_execution
    };
    let kept_groups_ms = merge_ms(None);
    assert_eq!(merge_ms(Some(vec![0, 1])), kept_groups_ms);
    assert_eq!(merge_ms(Some(vec![1, 0])), kept_groups_ms);
    let rollup_ms = merge_ms(Some(vec![1]));
    assert!(
        rollup_ms > 100.0 * kept_groups_ms,
        "rollup charged {rollup_ms:.3} ms, a pass-through {kept_groups_ms:.3} ms"
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
        raw_rows_span_ms: None,
        state_input_rows: None,
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
    ddsketch_build_cost(data, producer, source, column, unchanged_columns).0
}

/// [`ddsketch_state_bytes`] and the build's [`NodeCost`].
fn ddsketch_build_cost(
    data: &DataWorkload,
    producer: Payload,
    source: &str,
    column: &str,
    unchanged_columns: BTreeSet<&str>,
) -> (u64, NodeCost) {
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
        raw_rows_span_ms: None,
        state_input_rows: None,
    };
    let settings = CostInputs::default();
    let usage =
        node_resource_usage_and_cost(&build, &[producer_output], (1, 1.0), None, data, &settings)
            .unwrap();
    (usage.output.bytes, usage.cost)
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
            distinct_values: None,
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
    // 600 rows span less than the declared seven decades, so the declared
    // range does not lower their bytes; it never raises them.
    assert!(ddsketch_costed_bytes(false, Some(latency)) <= ddsketch_costed_bytes(false, None));
}

/// A DDSketch over a scan of `latency` whose `value` declares no
/// `column_magnitudes` is costed over the assumed nine decades, and its
/// node cost names that assumption: the source, the column and the range
/// [1, 1e9]. The DDSketch store grows without collapsing buckets, so its α
/// holds whatever the range and the assumption is about cost only.
/// Declared, there is none.
#[test]
fn a_ddsketch_over_an_undeclared_magnitude_range_names_the_assumed_range() {
    use asap_types::workload::ValueMagnitudeRange;
    use asap_types::workload::{AssumedDataFact, DataFactAssumption, SourceFacts};
    let unchanged = || BTreeSet::from(["ts", "value"]);
    let (_, undeclared) = ddsketch_build_cost(
        &DataWorkload::default(),
        metric_scan("latency"),
        "latency",
        "value",
        unchanged(),
    );
    let assumed = AssumedDataFact {
        sources: vec!["latency".to_string()],
        assumption: DataFactAssumption::DDSketchValueMagnitudeRange {
            column: Some("value".to_string()),
            assumed_range: ValueMagnitudeRange {
                min_abs: 1.0,
                max_abs: 1e9,
            },
        },
    };
    assert_eq!(undeclared.assumed_data_facts, vec![assumed.clone()]);
    assert!(!assumed.accuracy_target_depends_on_it());
    let declared = declaring(
        "latency",
        SourceFacts {
            column_magnitudes: BTreeMap::from([(
                "value".to_string(),
                ValueMagnitudeRange {
                    min_abs: 0.001,
                    max_abs: 10_000.0,
                },
            )]),
            ..SourceFacts::default()
        },
    );
    let (_, declared) = ddsketch_build_cost(
        &declared,
        metric_scan("latency"),
        "latency",
        "value",
        unchanged(),
    );
    assert!(declared.assumed_data_facts.is_empty());
}

/// Two sources folded into one DDSketch, one declaring its `value`'s
/// magnitudes and one not: the assumption names only the source that
/// declares none, since the union of their ranges is unknown.
#[test]
fn a_ddsketch_assumption_names_only_the_sources_that_declare_no_range() {
    use asap_types::workload::{DataFactAssumption, SourceFacts, ValueMagnitudeRange};
    let declared_range = ValueMagnitudeRange {
        min_abs: 0.001,
        max_abs: 10.0,
    };
    let data = DataWorkload {
        sources: BTreeMap::from([(
            "a".to_string(),
            SourceFacts {
                column_magnitudes: BTreeMap::from([("value".to_string(), declared_range)]),
                ..SourceFacts::default()
            },
        )]),
        ..DataWorkload::default()
    };
    let facts = ddsketch_assumed_facts_over_sources(&data, &["a", "b"]);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].sources, vec!["b".to_string()]);
    assert!(matches!(
        facts[0].assumption,
        DataFactAssumption::DDSketchValueMagnitudeRange { .. }
    ));
}

/// The assumed facts of a DDSketch build over `value` of a producer that
/// reads every source of `sources` and passes `value` on unchanged.
fn ddsketch_assumed_facts_over_sources(
    data: &DataWorkload,
    sources: &[&str],
) -> Vec<asap_types::workload::AssumedDataFact> {
    use asap_types::ir::schema::{DataType, Field};
    use asap_types::summary_type::SketchAlgorithm;
    let producer = PhysicalASAPDAGNode {
        output_schema: Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ]),
        ..node(metric_scan(sources[0]))
    };
    let build = PhysicalASAPDAGNode {
        parameters: Some(SketchParams::DDSketch { alpha: 0.01 }),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::Named("value".to_string())),
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
        sources: sources.iter().copied().collect(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::from(["ts", "value"]),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        state_input_rows: None,
        raw_rows_span_ms: None,
    };
    let settings = CostInputs::default();
    node_resource_usage_and_cost(&build, &[producer_output], (1, 1.0), None, data, &settings)
        .unwrap()
        .cost
        .assumed_data_facts
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

/// A schema of a window's time, `state` of `state_type` and the labels
/// `labels` (strings).
fn state_schema(state_type: FieldDataType, labels: &[&str]) -> Schema {
    use asap_types::ir::schema::DataType;
    let mut fields = vec![
        Field::new(
            "window_end",
            FieldDataType::Plain(DataType::Timestamp),
            false,
        ),
        Field::new("state", state_type, false),
    ];
    fields.extend(
        labels
            .iter()
            .map(|label| Field::new(*label, FieldDataType::Plain(DataType::Utf8), false)),
    );
    Schema::new(fields)
}

/// A query-time merge of a 1h `TimeRange` over an ingestion-time chain of
/// KLL states in 10 s tumbling windows of 1,000 series sampled at 1 Hz
/// (`quantile_over_time(0.99, latency[1h])` at a 1% rank target): CPU
/// operations per second of the merge at KLL `k`.
fn kll_merge_cpu_ops(k: u32) -> f64 {
    kll_merge_cpu_ops_over(k, 10_000, 3_600)
}

/// [`kll_merge_cpu_ops`] over windows of `window_ms` and a `TimeRange` of
/// `range_s` seconds: at one execution every 10 s, the CPU operations per
/// second of a merge of `range_s` / `window_ms` states per series.
fn kll_merge_cpu_ops_over(k: u32, window_ms: u64, range_s: u64) -> f64 {
    use asap_types::ir::properties::ExecutionDataState;
    let settings = CostInputs::default();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 1000.0}},
        "sources": {"latency": {"series_count": 1000, "metric_type": "gauge",
            "column_signs": {"value": "non_negative"}, "sample_interval": 1000}}
    }))
    .unwrap();
    let scan_schema = Schema::default();
    let scan = node(Payload::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: scan_schema,
    }));
    let scan_output = ProducerOutput {
        payload: &scan.payload,
        output_schema: &scan.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 48_000,
        },
        sources: ["latency"].into(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: None,
    };
    let mut chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: 0,
        summary_type: FieldDataType::Sketch(asap_types::summary_type::SketchAlgorithm::Kll),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: window_ms,
    }));
    chain.output_state = ExecutionDataState::INGESTION_ROWS;
    chain.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
    chain.retention_ms = Some(range_s * 1_000);
    chain.parameters = Some(SketchParams::Kll { k });
    chain.output_schema = state_schema(
        FieldDataType::Sketch(asap_types::summary_type::SketchAlgorithm::Kll),
        &[asap_types::ir::schema::PROMQL_SERIES_IDENTITY],
    );
    let chain_usage = node_resource_usage_and_cost(
        &chain,
        &[scan_output],
        (1, 1.0),
        Some(window_ms),
        &data,
        &settings,
    )
    .unwrap();
    let mut windows = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(range_s),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    windows.output_schema = chain.output_schema.clone();
    let windows_usage = node_resource_usage_and_cost(
        &windows,
        &[produced(&chain, &chain_usage)],
        (1, 0.1),
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(
        windows_usage.output.rows,
        1_000 * range_s * 1_000 / window_ms
    );
    let mut merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    merge.parameters = Some(SketchParams::Kll { k });
    merge.output_schema = chain.output_schema.clone();
    let merge_usage = node_resource_usage_and_cost(
        &merge,
        &[produced(&windows, &windows_usage)],
        (1, 0.1),
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(merge_usage.output.rows, 1_000);
    merge_usage.cpu_ops_per_second
}

/// Stage 2 offers stored KLL windows of the same shape at k = 1,404 and
/// k = 547 for `quantile_over_time(0.99, latency[1h])` at a 1% rank
/// target. Merging each series' 360 windows of 10 rows measures 4.8 ms per
/// series at k = 1,404 and 1.7 ms at k = 547 in the executor, since each
/// window is merged into the group's state, which grows towards its `k`. A
/// merge charged per state alone cost both the same. The charge per series
/// is within 2x of each measurement, the merged state charged at the bytes
/// the planner gives it (17.0 KB at k = 547, where the library holds 8.9).
#[test]
fn a_kll_merge_costs_more_at_a_larger_k() {
    let small = kll_merge_cpu_ops(547);
    let large = kll_merge_cpu_ops(1_404);
    assert!(
        large > 1.5 * small,
        "k = 1,404: {large} ops/s, k = 547: {small}; measured 2.8x"
    );
    // Operations per second at one merge every 10 s, over 1,000 series, at
    // the default 1e-6 ms per operation.
    let ms_per_series = |ops_per_second: f64| ops_per_second * 10.0 / 1_000.0 * 1e-6;
    for (k, ops_per_second, measured_ms) in [(547, small, 1.7), (1_404, large, 4.8)] {
        let charged_ms = ms_per_series(ops_per_second);
        assert!(
            (0.5..=2.0).contains(&(charged_ms / measured_ms)),
            "k = {k}: charged {charged_ms:.2} ms per series, measured {measured_ms}"
        );
    }
}

/// Finalizing 1,000 exact accumulators into their output rows measures
/// 2.5 µs per group (1.8–15 over 135 nodes); charged as one row's update
/// (40 ns) it made stored windows look cheaper than a query-time
/// accumulator grouped into 10 rows.
#[test]
fn a_finalize_charges_each_group_its_measured_time() {
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let merged = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    }));
    let merged_output = ProducerOutput {
        payload: &merged.payload,
        output_schema: &merged.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 56_000,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Exact(&ExactKind::Sum),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: None,
    };
    let finalize = node(Payload::ASAP(ASAPOp::FinalizeExactAccumulator { child: 0 }));
    let usage = node_resource_usage_and_cost(
        &finalize,
        &[merged_output],
        (1, 1.0),
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert!(
        usage.work_ms_per_execution >= 1_000.0 * 2.0e-3,
        "finalize of 1,000 groups charged {} ms, measured 2.5",
        usage.work_ms_per_execution
    );
}

/// A per-series exact aggregate that carries `job` beside the series
/// identity (under `sum by (job) (sum_over_time(latency[5m]))`) measures
/// 51–77 ns per row, one carrying nothing 18–38: the carried label costs
/// 37 ns per row more.
#[test]
fn a_per_series_aggregate_charges_the_labels_it_carries() {
    use asap_types::ir::operator::AggIntent;
    use asap_types::ir::schema::{DataType, PROMQL_SERIES_IDENTITY};
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let rows = node(Payload::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        child: 0,
    }));
    let rows_output = ProducerOutput {
        payload: &rows.payload,
        output_schema: &rows.output_schema,
        output: EdgeStatistics {
            rows: 300_000,
            bytes: 300_000 * 48,
        },
        sources: BTreeSet::new(),
        family: StateFamily::None,
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: None,
    };
    let ms_per_row = |labels: &[&str]| {
        let mut aggregate = node(Payload::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
            child: 0,
        }));
        let mut fields = vec![
            Field::new("ts", FieldDataType::Plain(DataType::Timestamp), false),
            Field::new("value", FieldDataType::Plain(DataType::Float64), false),
        ];
        fields.extend(
            labels
                .iter()
                .map(|label| Field::new(*label, FieldDataType::Plain(DataType::Utf8), false)),
        );
        aggregate.output_schema = Schema::new(fields);
        node_resource_usage_and_cost(
            &aggregate,
            std::slice::from_ref(&rows_output),
            (1, 1.0),
            None,
            &data,
            &settings,
        )
        .unwrap()
        .work_ms_per_execution
            / 300_000.0
    };
    let carrying_job = ms_per_row(&["job", PROMQL_SERIES_IDENTITY]);
    let carrying_nothing = ms_per_row(&[PROMQL_SERIES_IDENTITY]);
    let carried_ns = (carrying_job - carrying_nothing) * 1e6;
    assert!(
        (30.0..=45.0).contains(&carried_ns),
        "the carried job costs {carried_ns} ns per row, measured 37"
    );
}

/// A merge of few KLL states per group pays more per group than its
/// states' merges: `quantile_over_time(0.99, latency[5m])` at a 1% rank
/// target over stored 60 s windows of 1,000 series at 1 Hz merges 5
/// states of 60 rows per series, which the executor measures at 33 µs per
/// series (32 at 10,000 series), where the states' merges alone are
/// charged 14.
#[test]
fn a_kll_merge_of_few_states_charges_each_group_its_measured_time() {
    // One merge every 10 s over 1,000 series, at 1e-6 ms per operation.
    let us_per_series = kll_merge_cpu_ops_over(547, 60_000, 300) * 10.0 / 1_000.0 * 1e-3;
    assert!(
        (0.5..=2.0).contains(&(us_per_series / 33.0)),
        "charged {us_per_series:.1} µs per series, measured 33"
    );
}

/// Merging exact Max (or Min) states measures 113–153 ns per state over
/// 100 to 10,000 groups (a fit of 125 per state and 0.86 µs per group);
/// Sum and Count states measure about 230.
#[test]
fn a_max_merge_charges_its_measured_time_per_state() {
    use crate::node::operations_per_unit::merge_operations_per_state;
    for kind in [ExactKind::Max, ExactKind::Min] {
        let max = merge_operations_per_state(StateFamily::Exact(&kind), 56, 56, None, 1);
        assert!((100.0..=150.0).contains(&max), "{kind:?}: {max} per state");
    }
    let sum = merge_operations_per_state(StateFamily::Exact(&ExactKind::Sum), 56, 56, None, 1);
    assert!((200.0..=260.0).contains(&sum), "Sum: {sum} per state");
}

/// A merge over several inputs sizes each group's merged state for the
/// rows every input's states fold, not the first input's alone: over
/// 1,000 series, one input gives each series 360 KLL windows of 10 rows
/// and the other 60 windows of 100 rows, so its merged state folds
/// 3,600 + 6,000 = 9,600 rows (sized from the first input alone, 420
/// states of 10 rows: 4,200), at k = 4,000 more bytes than 4,200 rows
/// take.
#[test]
fn a_merge_over_several_inputs_sizes_its_state_for_all_of_them() {
    use crate::node::state_bytes::{sketch_state_bytes, StateInputRows};
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let k = 4_000;
    let windows = PhysicalASAPDAGNode {
        output_schema: state_schema(
            FieldDataType::Sketch(asap_types::summary_type::SketchAlgorithm::Kll),
            &[asap_types::ir::schema::PROMQL_SERIES_IDENTITY],
        ),
        ..node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(3_600),
            kind: TimeRangeKind::Range,
            child: 0,
        }))
    };
    let rows = StateInputRows {
        rows_per_state: Some(10),
        non_negative: true,
        value_magnitude_range: None,
        distinct_values: None,
    };
    let input = ProducerOutput {
        payload: &windows.payload,
        output_schema: &windows.output_schema,
        output: EdgeStatistics {
            rows: 360_000,
            bytes: 360_000 * 944,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Sketch(&SketchParams::Kll { k }),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 360,
        chain_tumbling_window_width_ms: Some(10_000),
        raw_rows_span_ms: None,
        state_input_rows: Some(rows),
    };
    let wider = ProducerOutput {
        output: EdgeStatistics {
            rows: 60_000,
            bytes: 60_000 * 1_328,
        },
        tumbling_windows: 60,
        chain_tumbling_window_width_ms: Some(60_000),
        raw_rows_span_ms: None,
        state_input_rows: Some(StateInputRows {
            rows_per_state: Some(100),
            ..rows
        }),
        ..input.clone()
    };
    let mut merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
        children: vec![0, 1],
        group_by: None,
    }));
    merge.parameters = Some(SketchParams::Kll { k });
    merge.output_schema = windows.output_schema.clone();
    let usage =
        node_resource_usage_and_cost(&merge, &[input, wider], (1, 0.1), None, &data, &settings)
            .unwrap();
    assert_eq!(usage.output.rows, 1_000);
    let merged = StateInputRows {
        rows_per_state: Some(9_600),
        ..rows
    };
    assert_eq!(
        usage.state_input_rows.and_then(|rows| rows.rows_per_state),
        Some(9_600)
    );
    let from_first_alone = StateInputRows {
        rows_per_state: Some(4_200),
        ..rows
    };
    assert!(
        sketch_state_bytes(&SketchParams::Kll { k }, merged)
            > sketch_state_bytes(&SketchParams::Kll { k }, from_first_alone)
    );
    assert!(
        usage.output.bytes_per_row() > sketch_state_bytes(&SketchParams::Kll { k }, merged),
        "merged state sized for 9,600 rows"
    );
}

/// The KLL estimate of `quantile_over_time(0.99, latency[5m])` at a 1%
/// rank target (k = 547) over 1,000 series, per group, after `producer`
/// outputs states of `rows_per_state` rows each, charged `state_bytes`.
fn kll_estimate_us_per_group(producer: Payload, rows_per_state: u64, state_bytes: u64) -> f64 {
    use asap_types::summary_type::{SketchParams, SketchStatistic};
    let settings = CostInputs::default();
    let data = DataWorkload::default();
    let kll = SketchParams::Kll { k: 547 };
    let states = node(producer);
    let state_output = ProducerOutput {
        payload: &states.payload,
        output_schema: &states.output_schema,
        output: EdgeStatistics {
            rows: 1_000,
            bytes: 1_000 * state_bytes,
        },
        sources: BTreeSet::new(),
        family: StateFamily::Sketch(&kll),
        unchanged_columns: BTreeSet::new(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: Some(StateInputRows {
            rows_per_state: Some(rows_per_state),
            non_negative: true,
            value_magnitude_range: None,
            distinct_values: None,
        }),
    };
    let estimate = node(Payload::ASAP(ASAPOp::SummaryEstimate {
        summary_input: 0,
        query: SketchStatistic::Quantile { q: 0.99 },
    }));
    let usage =
        node_resource_usage_and_cost(&estimate, &[state_output], (1, 1.0), None, &data, &settings)
            .unwrap();
    usage.work_ms_per_execution * 1e3 / 1_000.0
}

/// A KLL quantile sorts the items its state holds. A merge leaves each
/// level sorted, so reading a merged state measures 5.9 µs per group
/// (300 rows per series at k = 547, 1,000 series); a state built by
/// updates keeps its newest items unsorted, at most `k` of them, and
/// measures nearly the same at 300 rows per state (13.1–14.3 µs over 100
/// to 10,000 series) as at 3,000 (14.7–27.3, median 18.9). Charged per byte
/// of the state (4,844 bytes at 300 rows, 17,000 at 3,000), the 3,000-row
/// state was charged 33.1, 3x the 300-row one.
#[test]
fn a_kll_estimate_over_a_state_built_by_updates_charges_its_measured_time() {
    let merged = kll_estimate_us_per_group(
        Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: None,
        }),
        300,
        4_844,
    );
    assert!(
        (0.67..=1.5).contains(&(merged / 5.9)),
        "after a merge: charged {merged:.1} µs per group, measured 5.9"
    );
    let built = |rows_per_state, state_bytes| {
        kll_estimate_us_per_group(
            Payload::ASAP(ASAPOp::SummaryAgg {
                child: 0,
                summary_type: FieldDataType::Sketch(asap_types::summary_type::SketchAlgorithm::Kll),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            }),
            rows_per_state,
            state_bytes,
        )
    };
    let (few, many) = (built(300, 4_844), built(3_000, 17_000));
    for (rows, charged, measured) in [(300, few, 13.4), (3_000, many, 18.9)] {
        assert!(
            (0.67..=1.5).contains(&(charged / measured)),
            "after updates of {rows} rows: charged {charged:.1} µs per group, measured {measured}"
        );
    }
    assert!(
        many < 2.0 * few,
        "3,000 rows charged {many:.1} µs, 300 rows {few:.1}; measured 1.4x"
    );
}

/// What one counter-matrix sketch (Count-Min or Count Sketch with heap,
/// UnivMon) measured on the executor did: `groups` states of
/// `rows_per_state` rows each, every row one of `items` distinct items, as
/// [`counter_sketch_charges_ms`] charges them.
struct CounterSketchShape {
    parameters: SketchParams,
    groups: u64,
    rows_per_state: u64,
    items: u64,
}

/// What a [`CounterSketchShape`] is charged, in ms per execution: the
/// query-time build (a `SummaryAgg` keyed by `key`, one state per `g`, over
/// finalized rows, not raw ones), its readout (a top-10 or an L2), a merge
/// of every state into one, and the same build at ingestion time (a
/// tumbling-window chain of 60 s windows, each the shape's rows).
struct CounterSketchCharges {
    build_ms: f64,
    readout_ms: f64,
    merge_ms: f64,
    ingestion_build_ms: f64,
}

fn counter_sketch_charges_ms(shape: &CounterSketchShape) -> CounterSketchCharges {
    use asap_types::ir::schema::DataType;
    use asap_types::summary_type::{SketchAlgorithm, SummaryInputExpr};
    let settings = CostInputs::default();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 1000.0}},
        "sources": {"t": {"distinct_values_per_label": {"g": shape.groups, "key": shape.items}}}
    }))
    .unwrap();
    let keyed = |weight| SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named("key".into()))),
        weight,
    };
    let value = || SummaryInputExpr::Column(ColumnRef::Named("value".into()));
    let top_10 = SketchStatistic::TopK { k: 10 };
    let (algorithm, update, statistic) = match shape.parameters {
        SketchParams::UnivMon { .. } => (
            SketchAlgorithm::UnivMon,
            keyed(SummaryInputExpr::Constant(1.0)),
            SketchStatistic::FrequencyL2,
        ),
        SketchParams::CmsWithHeap { .. } => (SketchAlgorithm::CmsWithHeap, keyed(value()), top_10),
        SketchParams::CountSketchWithHeap { .. } => {
            (SketchAlgorithm::CountSketchWithHeap, keyed(value()), top_10)
        }
        SketchParams::Cms { .. }
        | SketchParams::CountSketch { .. }
        | SketchParams::Hydra(_)
        | SketchParams::Kll { .. }
        | SketchParams::DDSketch { .. }
        | SketchParams::Hll { .. }
        | SketchParams::Kmv { .. } => panic!("a counter-matrix sketch measured on the executor"),
    };
    let rows_schema = Schema::new(vec![
        Field::new("g", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("key", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("value", FieldDataType::Plain(DataType::Float64), false),
    ]);
    let rows_node = PhysicalASAPDAGNode {
        output_schema: rows_schema.clone(),
        ..node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(60),
            kind: TimeRangeKind::Range,
            child: 0,
        }))
    };
    let rows = |count: u64| ProducerOutput {
        payload: &rows_node.payload,
        output_schema: &rows_node.output_schema,
        output: EdgeStatistics {
            rows: count,
            bytes: count * 48,
        },
        sources: BTreeSet::from(["t"]),
        family: StateFamily::None,
        unchanged_columns: ["g", "key", "value"].into(),
        tumbling_windows: 1,
        chain_tumbling_window_width_ms: None,
        raw_rows_span_ms: None,
        state_input_rows: None,
    };
    let family = FieldDataType::Sketch(algorithm);
    let states_schema = Schema::new(vec![
        Field::new("g", FieldDataType::Plain(DataType::Utf8), false),
        Field::new("state", family.clone(), false),
    ]);
    let build = PhysicalASAPDAGNode {
        parameters: Some(shape.parameters.clone()),
        output_schema: states_schema.clone(),
        ..node(Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            summary_type: family.clone(),
            input: update.clone(),
            reduction: Reduction::by(vec![0]),
            filter: None,
        }))
    };
    let all_rows = shape.groups * shape.rows_per_state;
    let build_usage =
        node_resource_usage_and_cost(&build, &[rows(all_rows)], (1, 1.0), None, &data, &settings)
            .unwrap();
    assert_eq!(build_usage.output.rows, shape.groups);
    let readout = node(Payload::ASAP(ASAPOp::SummaryEstimate {
        summary_input: 0,
        query: statistic,
    }));
    let readout_usage = node_resource_usage_and_cost(
        &readout,
        &[produced(&build, &build_usage)],
        (1, 1.0),
        None,
        &data,
        &settings,
    )
    .unwrap();
    let merge = PhysicalASAPDAGNode {
        parameters: Some(shape.parameters.clone()),
        ..node(Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: Some(vec![]),
        }))
    };
    let merge_usage = node_resource_usage_and_cost(
        &merge,
        &[produced(&build, &build_usage)],
        (1, 1.0),
        None,
        &data,
        &settings,
    )
    .unwrap();
    assert_eq!(merge_usage.output.rows, 1);
    // At ingestion time, one second of a 60 s window's rows.
    let mut chain = PhysicalASAPDAGNode {
        parameters: Some(shape.parameters.clone()),
        output_schema: states_schema,
        ..node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: family,
            input: update,
            reduction: Reduction::by(vec![0]),
            filter: None,
            tumbling_window_width_ms: 60_000,
        }))
    };
    chain.output_state = ExecutionDataState::INGESTION_ROWS;
    chain.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
    chain.retention_ms = Some(60_000);
    let chain_usage = node_resource_usage_and_cost(
        &chain,
        &[rows(all_rows.div_ceil(60))],
        (1, 1.0),
        Some(60_000),
        &data,
        &settings,
    )
    .unwrap();
    CounterSketchCharges {
        build_ms: build_usage.work_ms_per_execution,
        readout_ms: readout_usage.work_ms_per_execution,
        merge_ms: merge_usage.work_ms_per_execution,
        ingestion_build_ms: chain_usage.cpu_ops_per_second * 60.0 * 1e-6,
    }
}

/// A Count Sketch with heap built at query time costs more the wider it
/// is, as the executor measures it: the top-10 by `url` over 100 rows of
/// 100 URLs into one state at depth 81 measures 3.08 ms at width 300 and
/// 6.35 ms at width 30,000, which allocates and zeroes 2.4 M counters more
/// (the executor's per-group keyed build, thread CPU, median of 3 runs on
/// one pinned core). Charged per row alone, both cost 0.15 ms. The ratio
/// of the charges is within the build fit's leave-one-setting-out error
/// (0.313 RMS log, a factor of 1.37) of the measured ratio.
#[test]
fn a_wider_query_time_count_sketch_costs_more_as_the_executor_measures() {
    let charged = |width| {
        counter_sketch_charges_ms(&CounterSketchShape {
            parameters: SketchParams::CountSketchWithHeap {
                width,
                depth: 81,
                heap_size: 10,
            },
            groups: 1,
            rows_per_state: 100,
            items: 100,
        })
        .build_ms
    };
    let (narrow, wide) = (charged(300), charged(30_000));
    let measured_ratio = 6.351 / 3.076;
    assert!(
        wide > narrow && (1.0 / 1.37..=1.37).contains(&(wide / narrow / measured_ratio)),
        "width 300 charged {narrow:.3} ms (measured 3.076), width 30,000 {wide:.3} ms (measured 6.351)"
    );
}

/// Counter-matrix sketches measured on the executor, at query and at
/// ingestion time, are charged within the fit's range of measured ÷
/// charged over its whole grid (235 builds, 158 readouts, 126 merges, 17
/// ingestion builds; see `operations_per_unit`), rounded outwards: builds
/// 0.54–4.94, readouts of 10 states or more 0.65–2.66, merges 0.79–2.76,
/// ingestion builds 0.61–2.37.
/// Charged per row alone, without a state's counters, its heap's
/// re-estimates or its size, the builds were 12–800x under.
#[test]
fn counter_matrix_sketches_are_charged_their_measured_time() {
    let cms = |width, depth| SketchParams::CmsWithHeap {
        width,
        depth,
        heap_size: 10,
    };
    let count_sketch = |width, depth| SketchParams::CountSketchWithHeap {
        width,
        depth,
        heap_size: 10,
    };
    let univmon = |sketch_cols| SketchParams::UnivMon {
        heap_size: 1,
        sketch_rows: 5,
        sketch_cols,
        layers: 1,
    };
    let shape = |parameters, groups, rows_per_state, items| CounterSketchShape {
        parameters,
        groups,
        rows_per_state,
        items,
    };
    // (term, shape, measured ms)
    let points = [
        ("build", shape(count_sketch(300, 81), 1, 100, 100), 3.076),
        ("build", shape(count_sketch(30_000, 81), 1, 100, 100), 6.351),
        (
            "build",
            shape(count_sketch(7_500, 81), 100, 100, 100),
            670.96,
        ),
        ("build", shape(cms(272, 5), 100, 100, 100), 32.13),
        ("build", shape(cms(2_719, 12), 1_000, 10, 10), 150.92),
        ("build", shape(univmon(624), 1, 100_000, 10_000), 62.29),
        ("build", shape(univmon(56_817), 100, 1_000, 1_000), 208.99),
        ("readout", shape(cms(1_360, 12), 1_000, 10, 10), 15.53),
        (
            "readout",
            shape(count_sketch(7_500, 81), 100, 100, 100),
            7.10,
        ),
        ("merge", shape(cms(2_719, 12), 1_000, 10, 10), 91.28),
        (
            "merge",
            shape(count_sketch(7_500, 81), 100, 100, 100),
            192.73,
        ),
        ("merge", shape(univmon(56_817), 100, 100, 100), 97.16),
        (
            "ingestion build",
            shape(cms(2_719, 12), 100, 100, 100),
            73.09,
        ),
        ("ingestion build", shape(univmon(624), 1_000, 10, 10), 9.99),
        (
            "ingestion build",
            shape(count_sketch(300, 81), 100, 100, 100),
            416.44,
        ),
    ];
    for (term, shape, measured) in points {
        let charges = counter_sketch_charges_ms(&shape);
        let (charged, range) = match term {
            "build" => (charges.build_ms, 0.54..=4.94),
            "readout" => (charges.readout_ms, 0.65..=2.66),
            "merge" => (charges.merge_ms, 0.79..=2.76),
            _ => (charges.ingestion_build_ms, 0.61..=2.37),
        };
        assert!(
            range.contains(&(measured / charged)),
            "{term} of {:?}, {} states of {} rows of {} items: charged {charged:.3} ms, measured {measured}",
            shape.parameters,
            shape.groups,
            shape.rows_per_state,
            shape.items
        );
    }
}

/// A heap's top-k read and its merge estimate the items the heap holds,
/// which are the distinct items its state folded, at most its size: a heap
/// of 10 over 1,000 rows of 5 URLs holds 5, as its updates count them, not
/// 10. Its read is charged 55 operations per counter of depth less for each
/// item it does not hold, its merge 53.
#[test]
fn a_heap_over_few_distinct_items_is_read_and_merged_for_the_items_it_holds() {
    let charges = |items| {
        counter_sketch_charges_ms(&CounterSketchShape {
            parameters: SketchParams::CmsWithHeap {
                width: 272,
                depth: 10,
                heap_size: 10,
            },
            groups: 10,
            rows_per_state: 1_000,
            items,
        })
    };
    let (few, many) = (charges(5), charges(10_000));
    let fewer_residents_ms = |per_counter: f64| 10.0 * 5.0 * per_counter * 10.0 * 1e-6;
    assert!(
        (many.readout_ms - few.readout_ms - fewer_residents_ms(55.0)).abs() < 1e-9,
        "readout {} ms over 5 items, {} over 10,000",
        few.readout_ms,
        many.readout_ms
    );
    assert!(
        (many.merge_ms - few.merge_ms - fewer_residents_ms(53.0)).abs() < 1e-9,
        "merge {} ms over 5 items, {} over 10,000",
        few.merge_ms,
        many.merge_ms
    );
}
