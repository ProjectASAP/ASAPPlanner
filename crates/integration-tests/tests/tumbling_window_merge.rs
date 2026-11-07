//! Tumbling windows merged by `SummaryMerge` answer the same per-series query
//! as one build over the whole window (#509 Example 3/4, Pattern B): tumbling windows
//! written by hand, and the planner's window-composition rule's
//! `SummaryTumblingWindowChain`, expanded into the tumbling windows the executor runs.
use crate::physical_common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::physical_planner::test_support::views_at;
use asap_executor::physical_planner::{compile, promql_rows, CompiledPhysicalDAG};
use asap_executor::runtime::Scope;
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::{explicit_local_candidates, explicit_summary_candidate};
use asap_logical_asap_optimization::test_support::{
    tumbling_window_candidates, window_composition_pass2_candidates,
    window_composition_shared_chain_candidates,
};
use asap_types::ir::operator::operator_properties::TimeShift;
use asap_types::ir::operator::tumbling_window_chain::expand_tumbling_window_chains;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::Schema;
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NodeToTimedNodeCache,
    NonASAPOp, Operator, OperatorNode, QueryRoot,
};
use asap_types::physical::{Materialization, Medium};
use asap_types::test_support::compile_physical_asap_dag_with_node_ids;

const EVALUATION_MS: i64 = 300_000;
const TUMBLING_WINDOW_WIDTH_MS: i64 = 60_000;
const TUMBLING_WINDOW_COUNT: i64 = 5;

/// An explicit supported single-build fixture for the tumbling-window merge rules.
fn single_build(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let root = lower_promql(query, accuracy.clone()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    if accuracy.is_exact() {
        explicit_local_candidates(root)
            .into_iter()
            .find(|candidate| {
                OperatorNode::reachable(candidate)
                    .iter()
                    .any(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
            })
            .expect("an exact accumulator fixture")
    } else {
        // Pin DDS so the actual positive-sample interpolation and tumbling-window merge
        // checks do not depend on the optimizer's preferred implementation.
        explicit_summary_candidate(root, asap_types::ir::schema::SketchAlgorithm::DDSketch)
    }
}

/// The per-entity `SummaryAgg` of `root` over its raw `TimeRange`.
fn summary_agg(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::reachable(root)
        .into_iter()
        .find(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .expect("a summary build")
}

/// Rewrite `root` so its summary is built in tumbling windows: tumbling window `i` is
/// `TimeRange(1m)` over `TimeShift(i·1m)` over the same `Scan`, and a
/// `SummaryMerge` combines the tumbling windows. Returns the root and the tumbling windows.
fn tumbling_windows(root: &Rc<OperatorNode>) -> (Rc<OperatorNode>, Vec<Rc<OperatorNode>>) {
    let agg = summary_agg(root);
    let Some(NonASAPOp::TimeRange { kind, child, .. }) = agg.children()[0].non_asap() else {
        panic!("expected a raw time range under the summary");
    };
    let tumbling_windows: Vec<_> = (0..TUMBLING_WINDOW_COUNT)
        .map(|i| {
            let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms: i * TUMBLING_WINDOW_WIDTH_MS,
                    at: None,
                },
                child: child.clone(),
            }))
            .unwrap();
            let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
                range: std::time::Duration::from_millis(TUMBLING_WINDOW_WIDTH_MS as u64),
                kind: *kind,
                child: shifted,
            }))
            .unwrap();
            OperatorNode::new_shared(agg.operator.map_children(|_| range.clone())).unwrap()
        })
        .collect();
    let merged = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: tumbling_windows.clone(),
        newest_tumbling_windows: None,
        group_by: None,
    }))
    .unwrap();
    let root = replace(root, &agg, &merged);
    root.validate_structure().unwrap();
    (root, tumbling_windows)
}

/// `root` with its summary built in tumbling windows by the planner's
/// window-composition rule, for a query repeating every minute: the
/// planned root (a `SummaryMerge` over one `SummaryTumblingWindowChain`) and its expansion
/// into the tumbling windows the executor runs.
fn planned_tumbling_windows(root: &Rc<OperatorNode>) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
    let roots = [QueryRoot::Operator(Rc::clone(root))];
    let rule_candidates =
        tumbling_window_candidates(&roots, &[Some(TUMBLING_WINDOW_WIDTH_MS as u64)]);
    let [rule_candidate] = rule_candidates.as_slice() else {
        panic!("one build to split: {rule_candidates:?}")
    };
    assert_eq!(
        (
            rule_candidate.tumbling_window_width_ms,
            rule_candidate.tumbling_window_count
        ),
        (TUMBLING_WINDOW_WIDTH_MS as u64, 5)
    );
    let mut pass2_candidates =
        window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
    let pass2_candidate = pass2_candidates.pop().unwrap();
    assert!(pass2_candidates.is_empty());
    let QueryRoot::Operator(planned) = pass2_candidate.roots.unwrap().remove(0) else {
        panic!("an operator root")
    };
    planned.validate_structure().unwrap();
    assert_eq!(
        planned.guarantee, root.guarantee,
        "merged tumbling windows keep the whole build's guarantee"
    );
    let expanded = expand_tumbling_window_chains(&planned).unwrap();
    expanded.validate_structure().unwrap();
    (planned, expanded)
}

/// `node` with `from` replaced by `to`.
fn replace(
    node: &Rc<OperatorNode>,
    from: &Rc<OperatorNode>,
    to: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    if Rc::ptr_eq(node, from) {
        return to.clone();
    }
    Rc::new(
        node.with_new_children(|child| replace(child, from, to))
            .unwrap()
            .with_guarantee(node.guarantee.clone()),
    )
}

/// One raw data sample: series, timestamp (ms) and value.
type RawDataSample = (&'static str, i64, f64);

/// Two series, four positive samples per minute in [0, 5m), each
/// `phase_ms` after its 15-s step (0: on the tumbling-window boundaries).
fn samples(phase_ms: i64) -> Vec<RawDataSample> {
    let mut rows = vec![];
    for (s, series) in ["a", "b"].into_iter().enumerate() {
        for minute in 0..TUMBLING_WINDOW_COUNT {
            for j in 0..4 {
                let ts = minute * TUMBLING_WINDOW_WIDTH_MS + j * 15_000 + phase_ms;
                rows.push((series, ts, (s * 100) as f64 + (minute * 4 + j + 1) as f64));
            }
        }
    }
    rows
}

struct Exported {
    dag: PhysicalASAPDAG,
    /// Each `SummaryAgg` node with its tumbling window.
    builds: Vec<(u64, i64)>,
}

fn export(root: &Rc<OperatorNode>, assignment: &MaterializationAssignment) -> Exported {
    let timed =
        apply_materialization_timings(root, assignment, &mut NodeToTimedNodeCache::new()).unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    let tumbling_window = |node: &OperatorNode| -> i64 {
        match node.children()[0].non_asap() {
            Some(NonASAPOp::TimeShift { shift, .. }) => shift.offset_ms / TUMBLING_WINDOW_WIDTH_MS,
            _ => 0,
        }
    };
    let mut builds = vec![];
    for node in &compiled.dag.nodes {
        let logical = compiled.node_ids.operator_node(node.id).unwrap();
        if let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &logical.operator {
            builds.push((node.id as u64, tumbling_window(child)));
        }
    }
    Exported {
        dag: compiled.dag,
        builds,
    }
}

fn schema_of(dag: &PhysicalASAPDAG, id: u64) -> Arc<Schema> {
    Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id as u64 == id)
            .unwrap()
            .output_schema
            .clone(),
    )
}

/// A store holding every sample as raw data at the `Scan` of `m`; each
/// `TimeRange` keeps its own window of them.
fn raw_store(dag: &PhysicalASAPDAG, samples: &[RawDataSample]) -> InMemoryDataStore {
    physical_common::raw_data_store(dag, |_, schema| {
        samples
            .iter()
            .map(|&(series, ts, value)| {
                let labels = BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    ("series".to_string(), series.to_string()),
                ]);
                promql_rows::series_row(schema, &labels, ts, value).unwrap()
            })
            .collect()
    })
}

fn run(plan: &CompiledPhysicalDAG, store: InMemoryDataStore, scope: Scope) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = physical_common::execute(plan, store, scope)
        .remove(0)
        .iter()
        .flat_map(|batch| batch.to_rows().to_vec())
        .collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

/// `Value` has no equality; its debug form identifies every scalar exactly.
fn render(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter().map(|row| format!("{row:?}")).collect()
}

fn has_timestamp(row: &[Value], ms: i64) -> bool {
    row.iter()
        .any(|v| matches!(v, Value::Timestamp(t) if *t == ms))
}

fn query_scope() -> Scope {
    Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    }
}

/// Compile `root` with everything at query time (Pattern B2: tumbling windows rebuilt
/// from raw rows at query time).
fn compile_at_query_time(
    root: &Rc<OperatorNode>,
) -> Result<(Exported, CompiledPhysicalDAG), String> {
    let exported = export(root, &MaterializationAssignment::default());
    let plan =
        compile(&exported.dag, &[], &[exported.dag.roots[0] as u64]).map_err(|e| e.to_string())?;
    Ok((exported, plan))
}

fn execute(root: &Rc<OperatorNode>, samples: &[RawDataSample]) -> Vec<Vec<Value>> {
    let (exported, plan) = compile_at_query_time(root).unwrap();
    run(&plan, raw_store(&exported.dag, samples), query_scope())
}

/// Build the tumbling windows in ingestion runs (stored `MatAtIngestion`, each with its
/// own build timestamp), then merge them in the query run (Pattern B1).
fn execute_retained(
    root: &Rc<OperatorNode>,
    tumbling_windows: &[Rc<OperatorNode>],
    samples: &[RawDataSample],
) -> Vec<Vec<Value>> {
    let mut assignment = MaterializationAssignment::default();
    for tumbling_window in tumbling_windows {
        assignment.set(
            tumbling_window,
            Materialization::MatAtIngestion(Medium::Memory),
        );
    }
    execute_retained_as(root, &assignment, samples)
}

/// The planner's tumbling-window chain maintained at ingestion time, as Stage 2's B1
/// option assigns it (`MatAtIngestion` on the chain), then expanded: each
/// tumbling window carries the chain's timing and materialization, so the tumbling windows are
/// built in ingestion runs and merged in the query run.
fn execute_maintained_chain(
    planned: &Rc<OperatorNode>,
    samples: &[RawDataSample],
) -> Vec<Vec<Value>> {
    let chain = OperatorNode::reachable(planned)
        .into_iter()
        .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryTumblingWindowChain { .. })))
        .expect("a tumbling-window chain");
    let mut assignment = MaterializationAssignment::default();
    assignment.set(&chain, Materialization::MatAtIngestion(Medium::Memory));
    let timed =
        apply_materialization_timings(planned, &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
    let expanded = expand_tumbling_window_chains(&timed).unwrap();
    for tumbling_window in OperatorNode::reachable(&expanded)
        .iter()
        .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
    {
        assert_eq!(
            tumbling_window.materialization,
            Materialization::MatAtIngestion(Medium::Memory)
        );
    }
    execute_retained_as(&expanded, &MaterializationAssignment::default(), samples)
}

fn execute_retained_as(
    root: &Rc<OperatorNode>,
    assignment: &MaterializationAssignment,
    samples: &[RawDataSample],
) -> Vec<Vec<Value>> {
    let mut exported = export(root, assignment);
    let builds: Vec<u64> = exported.builds.iter().map(|&(build, _)| build).collect();
    let views = views_at(&mut exported.dag, &builds).unwrap();
    // Each tumbling window's ingestion run refreshes its view from the raw samples.
    let mut store = InMemoryDataStore::default();
    for &(build, tumbling_window) in &exported.builds {
        let plan = compile(&exported.dag, &[], &[build]).unwrap();
        let end = EVALUATION_MS - tumbling_window * TUMBLING_WINDOW_WIDTH_MS;
        let rows = run(
            &plan,
            raw_store(&exported.dag, samples),
            Scope::Ingestion {
                window_start_ms: end - TUMBLING_WINDOW_WIDTH_MS,
                window_end_ms: end,
            },
        );
        assert!(rows.iter().all(|row| has_timestamp(row, end)));
        store = store
            .with_materialized_view(
                build as usize,
                vec![Batch::try_new(schema_of(&exported.dag, build), rows).unwrap()],
            )
            .unwrap();
    }
    let plan = compile(&exported.dag, &views, &[exported.dag.roots[0] as u64]).unwrap();
    run(&plan, store, query_scope())
}

fn values(rows: &[Vec<Value>]) -> Vec<f64> {
    rows.iter()
        .flat_map(|row| {
            row.iter().filter_map(|v| match v {
                Value::Float64(v) => Some(*v),
                _ => None,
            })
        })
        .collect()
}

/// Returns the per-series values, which every execution path agrees on.
fn assert_tumbling_window_merge_matches_single_build(
    query: &str,
    accuracy: AccuracyTarget,
    samples: &[RawDataSample],
) -> Vec<f64> {
    let single = single_build(query, accuracy);
    let (tumbling_windows, tumbling_window_builds) = tumbling_windows(&single);
    let expected = execute(&single, samples);
    assert_eq!(expected.len(), 2, "{expected:?}");
    assert!(expected.iter().all(|row| has_timestamp(row, EVALUATION_MS)));
    let result = values(&expected);
    let expected = render(&expected);
    assert_eq!(
        render(&execute(&tumbling_windows, samples)),
        expected,
        "tumbling windows rebuilt at query time"
    );
    assert_eq!(
        render(&execute_retained(
            &tumbling_windows,
            &tumbling_window_builds,
            samples
        )),
        expected,
        "retained tumbling windows"
    );
    let (planned, expanded) = planned_tumbling_windows(&single);
    assert_eq!(
        render(&execute(&expanded, samples)),
        expected,
        "the planner's tumbling-window chain, expanded"
    );
    assert_eq!(
        render(&execute_maintained_chain(&planned, samples)),
        expected,
        "the planner's tumbling-window chain, maintained at ingestion time"
    );
    let unexpanded = compile_at_query_time(&planned).err().unwrap_or_default();
    assert!(
        unexpanded.contains("SummaryTumblingWindowChain"),
        "{unexpanded}"
    );
    result
}

/// Exact per-series Sum over 5 merged 1-minute tumbling windows equals one 5-minute
/// build, timestamped at the evaluation time.
#[test]
fn exact_sum_over_tumbling_windows_matches_single_build() {
    let sums = assert_tumbling_window_merge_matches_single_build(
        "sum_over_time(m[5m])",
        AccuracyTarget::exact(),
        // 1 ms past each step: the window `(0, 5m]` holds all 20 samples.
        &samples(1),
    );
    assert_eq!(sums, vec![210., 2210.]);
}

/// The selected quantile sketch over 5 merged 1-minute tumbling windows equals one
/// 5-minute build, within the target of the exact p99 (20 and 120).
#[test]
fn quantile_over_tumbling_windows_matches_single_build() {
    let p99 = assert_tumbling_window_merge_matches_single_build(
        "quantile_over_time(0.99, m[5m])",
        AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative),
        &samples(0),
    );
    let mut p99 = p99;
    p99.sort_by(f64::total_cmp);
    assert_eq!(p99.len(), 2);
    for (estimate, exact) in p99.into_iter().zip([20., 120.]) {
        assert!(
            (estimate - exact).abs() <= 0.05 * exact,
            "{estimate} vs {exact}"
        );
    }
}

/// Exact counter Rate over 5 merged 1-minute tumbling windows equals one 5-minute
/// build: the merged increase is divided by the whole 5-minute window, not
/// by one tumbling window's minute.
#[test]
fn exact_rate_over_tumbling_windows_matches_single_build() {
    let rates = assert_tumbling_window_merge_matches_single_build(
        "rate(m[5m])",
        AccuracyTarget::exact(),
        &samples(0),
    );
    assert_eq!(rates.len(), 2);
    // 19 counted over 285 s of samples, extrapolated to the 5-min window.
    for rate in rates {
        assert!((rate - 20.0 / 300.0).abs() < 1e-9, "{rate}");
    }
}

/// With samples off the tumbling-window boundaries, Increase extrapolates to the
/// start of the whole 5-minute window, as one build does, not to the start
/// of the newest tumbling window.
#[test]
fn exact_increase_off_the_tumbling_window_boundaries_matches_single_build() {
    let increases = assert_tumbling_window_merge_matches_single_build(
        "increase(m[5m])",
        AccuracyTarget::exact(),
        &samples(7_000),
    );
    assert_eq!(increases.len(), 2);
}

/// The single build of `query`, evaluated at `evaluation_ms` over every
/// sample, as values.
fn whole_build_values_at(query: &str, evaluation_ms: i64, samples: &[RawDataSample]) -> Vec<f64> {
    let root = single_build(query, AccuracyTarget::exact());
    let (exported, plan) = compile_at_query_time(&root).unwrap();
    values(&run(
        &plan,
        raw_store(&exported.dag, samples),
        Scope::Query {
            evaluation_time_ms: evaluation_ms,
        },
    ))
}

/// A counter over `[5m] offset 1m` evaluated a minute later reads the same
/// window as one without the offset: the counter window ends at the
/// evaluation time minus the offset, not at the evaluation time.
#[test]
fn offset_counter_window_ends_at_the_offset() {
    let samples = samples(7_000);
    for function in ["rate", "increase"] {
        let plain = whole_build_values_at(&format!("{function}(m[5m])"), EVALUATION_MS, &samples);
        let shifted = whole_build_values_at(
            &format!("{function}(m[5m] offset 1m)"),
            EVALUATION_MS + TUMBLING_WINDOW_WIDTH_MS,
            &samples,
        );
        assert_eq!(shifted, plain, "{function}");
    }
}

/// A counter over `[5m] @ 300` reads the window ending at 300 s whatever the
/// evaluation time, and `@ 300 offset 1m` the one ending at 240 s: the same
/// values as the unanchored counter evaluated at 300 s and at 240 s.
#[test]
fn anchored_counter_window_ends_at_the_anchor() {
    let samples = samples(7_000);
    for function in ["rate", "increase"] {
        let at_300 = whole_build_values_at(&format!("{function}(m[5m])"), EVALUATION_MS, &samples);
        let at_240 = whole_build_values_at(
            &format!("{function}(m[5m])"),
            EVALUATION_MS - TUMBLING_WINDOW_WIDTH_MS,
            &samples,
        );
        assert_ne!(at_300, at_240, "{function}: the two windows differ");
        let anchored = whole_build_values_at(
            &format!("{function}(m[5m] @ 300)"),
            EVALUATION_MS + TUMBLING_WINDOW_WIDTH_MS,
            &samples,
        );
        assert_eq!(anchored, at_300, "{function} @ 300");
        let anchored_offset = whole_build_values_at(
            &format!("{function}(m[5m] @ 300 offset 1m)"),
            EVALUATION_MS + TUMBLING_WINDOW_WIDTH_MS,
            &samples,
        );
        assert_eq!(anchored_offset, at_240, "{function} @ 300 offset 1m");
    }
}

/// Tumbling windows whose ranges read the scan through two stacked `TimeShift`s (the
/// tumbling window's shift over a zero shift) compile and answer as the single build:
/// the per-entity build looks through every shift to its raw source.
#[test]
fn tumbling_windows_over_stacked_time_shifts_match_single_build() {
    let single = single_build("increase(m[5m])", AccuracyTarget::exact());
    let (tumbling_windows, _) = tumbling_windows(&single);
    let agg = summary_agg(&single);
    let Some(NonASAPOp::TimeRange { child: scan, .. }) = agg.children()[0].non_asap() else {
        panic!("expected a raw time range under the summary");
    };
    let zero_shift = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 0,
            at: None,
        },
        child: scan.clone(),
    }))
    .unwrap();
    let stacked = replace(&tumbling_windows, scan, &zero_shift);
    let expected = render(&execute(&single, &samples(0)));
    assert_eq!(expected.len(), 2, "{expected:?}");
    assert_eq!(render(&execute(&stacked, &samples(0))), expected);
}

/// Exact counter Increase over 5 merged 1-minute tumbling windows equals one 5-minute
/// build: the merge joins consecutive tumbling windows across their boundaries.
#[test]
fn exact_increase_over_tumbling_windows_matches_single_build() {
    let increases = assert_tumbling_window_merge_matches_single_build(
        "increase(m[5m])",
        AccuracyTarget::exact(),
        &samples(0),
    );
    assert_eq!(increases.len(), 2);
}

/// A middle tumbling window with no samples, for one series or for every series: the
/// merge joins the tumbling windows on either side of the empty one, and Increase
/// matches one 5-minute build.
#[test]
fn exact_increase_over_an_empty_middle_tumbling_window_matches_single_build() {
    let middle = 2 * TUMBLING_WINDOW_WIDTH_MS..3 * TUMBLING_WINDOW_WIDTH_MS;
    for emptied in [&["a"][..], &["a", "b"]] {
        let gapped: Vec<RawDataSample> = samples(0)
            .into_iter()
            .filter(|(series, ts, _)| !(emptied.contains(series) && middle.contains(ts)))
            .collect();
        let increases = assert_tumbling_window_merge_matches_single_build(
            "increase(m[5m])",
            AccuracyTarget::exact(),
            &gapped,
        );
        assert_eq!(increases.len(), 2, "{emptied:?}");
    }
}

/// A counter over `[5m] offset 1m` split into the planner's tumbling windows, read
/// at query time or maintained at ingestion time, matches one build: the
/// tumbling windows' shifts fold the outer offset, and the merged counter window ends
/// at the evaluation time minus the offset.
#[test]
fn a_counter_over_tumbling_windows_with_an_outer_offset_matches_single_build() {
    let samples = samples(7_000);
    for function in ["rate", "increase"] {
        let single = single_build(
            &format!("{function}(m[5m] offset 1m)"),
            AccuracyTarget::exact(),
        );
        let expected = render(&execute(&single, &samples));
        assert_eq!(expected.len(), 2, "{function}: {expected:?}");
        let (planned, expanded) = planned_tumbling_windows(&single);
        assert_eq!(
            render(&execute(&expanded, &samples)),
            expected,
            "{function}: expanded tumbling windows"
        );
        assert_eq!(
            render(&execute_maintained_chain(&planned, &samples)),
            expected,
            "{function}: tumbling windows maintained at ingestion time"
        );
    }
}

/// `root`'s summary built from tumbling windows `(shift, width)` merged by one
/// `SummaryMerge`, both in minutes, or the IR's refusal of that merge.
fn tumbling_windows_at(
    root: &Rc<OperatorNode>,
    tumbling_windows: &[(i64, i64)],
) -> Result<Rc<OperatorNode>, String> {
    let agg = summary_agg(root);
    let Some(NonASAPOp::TimeRange { kind, child, .. }) = agg.children()[0].non_asap() else {
        panic!("expected a raw time range under the summary");
    };
    let children = tumbling_windows
        .iter()
        .map(|&(shift, width)| {
            let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms: shift * TUMBLING_WINDOW_WIDTH_MS,
                    at: None,
                },
                child: child.clone(),
            }))
            .unwrap();
            let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
                range: std::time::Duration::from_millis((width * TUMBLING_WINDOW_WIDTH_MS) as u64),
                kind: *kind,
                child: shifted,
            }))
            .unwrap();
            OperatorNode::new_shared(agg.operator.map_children(|_| range.clone())).unwrap()
        })
        .collect();
    let merged = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        newest_tumbling_windows: None,
        group_by: None,
    }))
    .map_err(|error| format!("{error:?}"))?;
    Ok(replace(root, &agg, &merged))
}

/// Counter tumbling windows form one window only back to back: with a gap between
/// them, compiling the counter evaluation is an error; overlapping tumbling windows
/// are already refused by the `SummaryMerge`, whose counter inputs must be
/// disjoint in time.
#[test]
fn counter_tumbling_windows_with_a_gap_or_an_overlap_are_rejected() {
    let single = single_build("rate(m[5m])", AccuracyTarget::exact());
    let gapped = tumbling_windows_at(&single, &[(0, 1), (2, 1)]).unwrap();
    let error = compile_at_query_time(&gapped).err().unwrap_or_default();
    assert!(error.contains("ambiguous logical windows"), "{error}");
    let overlap = tumbling_windows_at(&single, &[(0, 2), (1, 2)])
        .err()
        .unwrap_or_default();
    assert!(overlap.contains("InputsOverlap"), "{overlap}");
}

/// A 2-min and a 5-min quantile, both every minute, on 1-min tumbling windows: the
/// window-composition rule offers one 5-tumbling-window chain both read, the 2-min
/// query merging only its newest 2 tumbling windows. Expanded, rebuilt at query time
/// or maintained at ingestion time, each query's answer from the shared
/// chain equals its answer from its own chain.
#[test]
fn newest_tumbling_windows_of_a_shared_chain_match_a_separate_chain() {
    let accuracy = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let roots = [
        QueryRoot::Operator(single_build(
            "quantile_over_time(0.5, m[2m])",
            accuracy.clone(),
        )),
        QueryRoot::Operator(single_build("quantile_over_time(0.99, m[5m])", accuracy)),
    ];
    let grid = Some(TUMBLING_WINDOW_WIDTH_MS as u64);
    let rule_candidates = tumbling_window_candidates(&roots, &[grid, grid]);
    assert_eq!(rule_candidates.len(), 2, "{rule_candidates:?}");
    let pass2_candidates = window_composition_pass2_candidates(&roots, &rule_candidates).unwrap();
    let both = |shared: bool| -> Vec<Rc<OperatorNode>> {
        pass2_candidates
            .iter()
            .find(|c| {
                c.tumbling_window_candidates.len() == 2 && c.shared_chains.is_empty() != shared
            })
            .expect("both builds in tumbling windows")
            .roots
            .as_ref()
            .unwrap()
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => Rc::clone(node),
                QueryRoot::Scalar(_) => panic!("an operator root"),
            })
            .collect()
    };
    let (shared, separate) = (both(true), both(false));
    let newest = OperatorNode::reachable(&shared[0])
        .into_iter()
        .find_map(|n| match n.asap() {
            Some(ASAPOp::SummaryMerge {
                newest_tumbling_windows,
                ..
            }) => Some(*newest_tumbling_windows),
            _ => None,
        });
    assert_eq!(newest, Some(Some(2)));
    let data = samples(0);
    for (query, (shared, separate)) in shared.iter().zip(&separate).enumerate() {
        let expected = execute(&expand_tumbling_window_chains(separate).unwrap(), &data);
        assert!(!expected.is_empty());
        assert_eq!(
            render(&execute(
                &expand_tumbling_window_chains(shared).unwrap(),
                &data
            )),
            render(&expected),
            "Q{}: the shared chain rebuilt at query time",
            query + 1
        );
        assert_eq!(
            render(&execute_maintained_chain(shared, &data)),
            render(&expected),
            "Q{}: the shared chain maintained at ingestion time",
            query + 1
        );
    }
}

/// Runs `root` at query time at `evaluation_ms` over every sample; each
/// `TimeRange` keeps its own window `(T - s - w, T - s]` for a range of
/// width `w` over a shift of `s` (none for an unshifted range).
fn execute_ranges_at_their_windows(
    root: &Rc<OperatorNode>,
    samples: &[RawDataSample],
    evaluation_ms: i64,
) -> Vec<Vec<Value>> {
    let (exported, plan) = compile_at_query_time(root).unwrap();
    let scope = Scope::Query {
        evaluation_time_ms: evaluation_ms,
    };
    run(&plan, raw_store(&exported.dag, samples), scope)
}

/// A 5-min quantile every 30 s (10 × 30-s tumbling windows) and a 5-min quantile
/// every minute (5 × 1-min tumbling windows): the rule offers one chain of 30-s
/// tumbling windows both read in full, each at its own interval. Expanded, each
/// query's answer from the shared chain equals one build over its window,
/// at 5 min, and for the 30-s query also at 4.5 min, off the 1-min grid.
#[test]
fn queries_at_different_intervals_read_one_chain_of_the_finer_tumbling_windows() {
    let accuracy = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let singles = [
        single_build("quantile_over_time(0.5, m[5m])", accuracy.clone()),
        single_build("quantile_over_time(0.99, m[5m])", accuracy),
    ];
    let roots: Vec<QueryRoot> = singles
        .iter()
        .map(|root| QueryRoot::Operator(Rc::clone(root)))
        .collect();
    let rule_candidates = tumbling_window_candidates(
        &roots,
        &[Some(30_000), Some(TUMBLING_WINDOW_WIDTH_MS as u64)],
    );
    let widths: Vec<u64> = rule_candidates
        .iter()
        .map(|c| c.tumbling_window_width_ms)
        .collect();
    assert_eq!(widths, [30_000, TUMBLING_WINDOW_WIDTH_MS as u64]);
    let shared = window_composition_pass2_candidates(&roots, &rule_candidates)
        .unwrap()
        .into_iter()
        .find(|c| c.tumbling_window_candidates.len() == 2 && !c.shared_chains.is_empty())
        .expect("one shared chain");
    // The whole set's shared chains, built alone, are the same candidates.
    let alone = window_composition_shared_chain_candidates(&roots, &rule_candidates);
    let first = alone.first().expect("one shared chain alone");
    assert_eq!(first.shared_chains, shared.shared_chains);
    assert_eq!(first.roots.as_ref().ok(), shared.roots.as_ref().ok());
    assert_eq!(
        (
            shared.shared_chains[0].tumbling_window_width_ms,
            shared.shared_chains[0].tumbling_window_count
        ),
        (30_000, 10)
    );
    let data = samples(0);
    let shared_roots: Vec<Rc<OperatorNode>> = shared
        .roots
        .unwrap()
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => expand_tumbling_window_chains(&node).unwrap(),
            QueryRoot::Scalar(_) => panic!("an operator root"),
        })
        .collect();
    for (query, evaluation_ms) in [(0, EVALUATION_MS), (1, EVALUATION_MS), (0, 270_000)] {
        let expected = execute_ranges_at_their_windows(&singles[query], &data, evaluation_ms);
        assert_eq!(expected.len(), 2, "{expected:?}");
        assert_eq!(
            render(&execute_ranges_at_their_windows(
                &shared_roots[query],
                &data,
                evaluation_ms
            )),
            render(&expected),
            "Q{} at {evaluation_ms} ms",
            query + 1
        );
    }
}

/// A 5-min quantile every minute (5 × 1-min tumbling windows) and a 3-min one every
/// 90 s (2 × 90-s tumbling windows): neither width divides the other, so the rule
/// offers one chain at their gcd, 10 × 30-s tumbling windows, the 3-min query merging
/// the newest 6. Expanded, each query's answer from the shared chain equals
/// one build over its window, at times on each query's own grid.
#[test]
fn queries_at_60_and_90_seconds_read_one_chain_of_30_second_tumbling_windows() {
    let accuracy = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let singles = [
        single_build("quantile_over_time(0.5, m[5m])", accuracy.clone()),
        single_build("quantile_over_time(0.99, m[3m])", accuracy),
    ];
    let roots: Vec<QueryRoot> = singles
        .iter()
        .map(|root| QueryRoot::Operator(Rc::clone(root)))
        .collect();
    let rule_candidates = tumbling_window_candidates(
        &roots,
        &[Some(TUMBLING_WINDOW_WIDTH_MS as u64), Some(90_000)],
    );
    let widths: Vec<(u64, u32)> = rule_candidates
        .iter()
        .map(|c| (c.tumbling_window_width_ms, c.tumbling_window_count))
        .collect();
    assert_eq!(widths, [(TUMBLING_WINDOW_WIDTH_MS as u64, 5), (90_000, 2)]);
    let shared = window_composition_pass2_candidates(&roots, &rule_candidates)
        .unwrap()
        .into_iter()
        .find(|c| c.tumbling_window_candidates.len() == 2 && !c.shared_chains.is_empty())
        .expect("one shared chain at the gcd");
    assert_eq!(
        (
            shared.shared_chains[0].tumbling_window_width_ms,
            shared.shared_chains[0].tumbling_window_count
        ),
        (30_000, 10)
    );
    let data = samples(0);
    let shared_roots: Vec<Rc<OperatorNode>> = shared
        .roots
        .unwrap()
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => expand_tumbling_window_chains(&node).unwrap(),
            QueryRoot::Scalar(_) => panic!("an operator root"),
        })
        .collect();
    for (query, evaluation_ms) in [(0, EVALUATION_MS), (1, 270_000), (1, 180_000)] {
        let expected = execute_ranges_at_their_windows(&singles[query], &data, evaluation_ms);
        assert_eq!(expected.len(), 2, "{expected:?}");
        assert_eq!(
            render(&execute_ranges_at_their_windows(
                &shared_roots[query],
                &data,
                evaluation_ms
            )),
            render(&expected),
            "Q{} at {evaluation_ms} ms",
            query + 1
        );
    }
}
