//! Tests of [`super::node_resource_usage_and_cost`].

use std::time::Duration;

use asap_types::ir::properties::ExecutionDataState;
use asap_types::ir::scalar::ColumnRef;
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

/// A DDSketch keeps a store for positive values, one for negative values
/// and a zero bucket (both +0.0 and -0.0 land in it). Values all ≥ 0 or
/// all ≤ 0 fill one store, values of both signs two; a sign-bit slip
/// (-0.0 where +0.0 was expected) still lands in the zero bucket, so the
/// store count follows the real-number sign of the values the sketch is
/// updated with: one store over a column of known sign, two over an
/// `Unknown` one. Today the cost decides it from its own lookup of a raw
/// producer over sources declared non-negative instead of the producer's
/// column signs.
mod ddsketch_stores_by_sign {
    use super::*;
    use asap_types::ir::operator::AggIntent;
    use asap_types::stages::ValueSign;
    use asap_types::summary_type::SketchAlgorithm;
    use asap_types::workload::{PrometheusMetricType, SourceFacts};
    use std::collections::BTreeMap;

    const ALPHA: f64 = 0.01;
    const INPUT_ROWS: u64 = 100_000;

    /// Sources `pos` (`value` declared non-negative), `neg` (declared
    /// non-positive), `mixed` (nothing declared), `c_total` (a counter)
    /// and the histogram family `h` (its `h_sum` undeclared).
    fn signed_sources() -> DataWorkload {
        let declared = |sign| SourceFacts {
            column_signs: BTreeMap::from([("value".to_string(), sign)]),
            ..SourceFacts::default()
        };
        let typed = |metric_type| SourceFacts {
            metric_type: Some(metric_type),
            ..SourceFacts::default()
        };
        DataWorkload {
            sources: BTreeMap::from([
                ("pos".to_string(), declared(ValueSign::NonNegative)),
                ("neg".to_string(), declared(ValueSign::NonPositive)),
                ("mixed".to_string(), SourceFacts::default()),
                ("c_total".to_string(), typed(PrometheusMetricType::Counter)),
                ("h".to_string(), typed(PrometheusMetricType::Histogram)),
            ]),
            ..DataWorkload::default()
        }
    }

    /// The bytes of one DDSketch state of α = 0.01 over all
    /// [`INPUT_ROWS`] rows (one group) holding one store (`one_store`) or
    /// two: the state-bytes formula with the store count chosen by hand.
    fn state_bytes_with(one_store: bool) -> u64 {
        sketch_shape(
            &SketchParams::DDSketch { alpha: ALPHA },
            StateInputRows {
                rows_per_state: Some(INPUT_ROWS),
                non_negative: one_store,
            },
        )
        .1
    }

    /// The bytes the cost prices for a query-time DDSketch build over all
    /// rows of `producer`, which reads `source`.
    fn priced_state_bytes(producer: Payload, source: &str) -> u64 {
        let settings = CostInputs::default();
        let data = signed_sources();
        let producer_node = node(producer);
        let input = ProducerOutput {
            payload: &producer_node.payload,
            output_schema: &producer_node.output_schema,
            output: EdgeStatistics {
                rows: INPUT_ROWS,
                bytes: INPUT_ROWS * 16,
            },
            sources: BTreeSet::from([source]),
            family: StateFamily::None,
            unchanged_columns: BTreeSet::new(),
            tumbling_windows: 1,
            chain_tumbling_window_width_ms: None,
        };
        let build = PhysicalASAPDAGNode {
            parameters: Some(SketchParams::DDSketch { alpha: ALPHA }),
            ..node(Payload::ASAP(ASAPOp::SummaryAgg {
                child: 0,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                filter: None,
            }))
        };
        node_resource_usage_and_cost(&build, &[input], (1, 1.0), None, &data, &settings)
            .unwrap()
            .output
            .bytes
    }

    /// The last five minutes of a scan: raw samples.
    fn last_five_minutes() -> Payload {
        Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        })
    }

    /// `rate(x[1m])` per series: not raw samples.
    fn rate_per_series() -> Payload {
        Payload::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec!["rate".into()],
            filters: vec![],
            having: None,
            child: 0,
        })
    }

    /// The two store counts price differently, so each test below can
    /// tell them apart.
    fn assert_store_counts_differ() {
        assert!(state_bytes_with(true) < state_bytes_with(false));
    }

    /// A DDSketch updated with `rate(c_total[1m])` per series (its
    /// producer the rate, as in `quantile_over_time(0.9,
    /// rate(c_total[1m])[5m:1m])`): a counter's rates are each ≥ +0, so
    /// one store, though its producer is not a raw scan.
    #[test]
    #[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
    fn b08_ddsketch_over_a_rate_of_a_counter_prices_one_store() {
        assert_store_counts_differ();
        assert_eq!(
            priced_state_bytes(rate_per_series(), "c_total"),
            state_bytes_with(true)
        );
    }

    /// Over `neg`, declared non-positive: every value ≤ 0, one store.
    #[test]
    #[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
    fn ddsketch_over_non_positive_values_prices_one_store() {
        assert_store_counts_differ();
        assert_eq!(
            priced_state_bytes(last_five_minutes(), "neg"),
            state_bytes_with(true)
        );
    }

    /// Over `mixed`, undeclared: values of both signs, two stores.
    #[test]
    #[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
    fn ddsketch_over_mixed_values_prices_two_stores() {
        assert_store_counts_differ();
        assert_eq!(
            priced_state_bytes(last_five_minutes(), "mixed"),
            state_bytes_with(false)
        );
    }

    /// Over `pos`, declared non-negative and read raw: one store, as
    /// today.
    #[test]
    #[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
    fn ddsketch_over_a_declared_raw_scan_is_unchanged() {
        assert_store_counts_differ();
        assert_eq!(
            priced_state_bytes(last_five_minutes(), "pos"),
            state_bytes_with(true)
        );
    }

    /// Over `h_sum`, the sum of an undeclared histogram family: a
    /// histogram may observe negative values (a temperature delta), so
    /// its sum may fall and have either sign: two stores. Today the cost
    /// reads `_sum` as a counter.
    #[test]
    #[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
    fn an_undeclared_histogram_sum_prices_two_stores() {
        // provisional answer Q8: only a declaration on the family's
        // `value` makes `h_sum` one-signed.
        assert_store_counts_differ();
        assert_eq!(
            priced_state_bytes(last_five_minutes(), "h_sum"),
            state_bytes_with(false)
        );
    }
}

/// The cost derives no sign of its own (sign test plan T6): the source
/// lookups `declared_non_negative`, `declared_counter` and
/// `raw_data_samples_non_negative` are gone from the cost crate and from
/// `asap_types::workload::bounds`, and the cost reads the producer's
/// `column_signs`. A source-text check of the non-test files.
#[test]
#[ignore = "PR S2: PhysicalASAPDAGNode.column_signs"]
fn cost_derives_no_sign() {
    use std::path::{Path, PathBuf};
    fn non_test_files(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" {
                    non_test_files(&path, files);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" {
                files.push(path);
            }
        }
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    non_test_files(&crate_dir.join("src"), &mut files);
    files.push(crate_dir.join("../types/src/workload/bounds.rs"));
    let mut violations = Vec::new();
    let mut reads_column_signs = false;
    for path in &files {
        let source = std::fs::read_to_string(path).unwrap();
        let in_cost = path.starts_with(crate_dir.join("src"));
        reads_column_signs |= in_cost && source.contains("column_signs");
        for derivation in [
            "declared_non_negative",
            "declared_counter",
            "raw_data_samples_non_negative",
        ] {
            if source.contains(derivation) {
                violations.push(format!("{}: {derivation}", path.display()));
            }
        }
    }
    if !reads_column_signs {
        violations.push("the cost crate reads no column_signs".into());
    }
    assert!(violations.is_empty(), "{violations:#?}");
}
