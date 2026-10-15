//! Tumbling panes merged by `SummaryMerge` answer the same per-series query
//! as one build over the whole window (#509 Example 3/4, Pattern B): panes
//! written by hand, and the planner's window-composition rule's
//! `PaneChain`, expanded into the panes the executor runs.
mod physical_common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_aware_mapping::asap_aware_cse::{pane_candidates, window_composition_pass2_candidates};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::selected_dag;
use asap_physical_operators::physical_planner::{
    compile, promql_rows, CompiledPhysicalDAG, InputContract,
};
use asap_physical_operators::runtime::Scope;
use asap_physical_operators::values::{Batch, Value};
use asap_types::ir::operator_properties::TimeShift;
use asap_types::ir::pane_chain::expand_pane_chains;
use asap_types::ir::physical_export::{compile_physical_asap_dag_with_node_ids, PhysicalASAPDAG};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, Materialization, MaterializationAssignment, Medium,
    NonASAPOp, Operator, OperatorNode, QueryRoot, TimingMemo,
};
use asap_types::post_asap::Schema;
use asap_types::types::{AccuracyTarget, ErrorMetric};

const EVALUATION_MS: i64 = 300_000;
const PANE_MS: i64 = 60_000;
const PANES: i64 = 5;

/// The selected single-build plan for a 5-minute per-series PromQL query.
fn single_build(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let root = lower_promql(query, accuracy.clone()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    selected_dag(root, accuracy)
}

/// The per-entity `SummaryAgg` of `root` over its raw `TimeRange`.
fn summary_agg(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::reachable(root)
        .into_iter()
        .find(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .expect("a summary build")
}

/// Rewrite `root` so its summary is built in tumbling panes: pane `i` is
/// `TimeRange(1m)` over `TimeShift(i·1m)` over the same `Scan`, and a
/// `SummaryMerge` combines the panes. Returns the root and the panes.
fn tumbling_panes(root: &Rc<OperatorNode>) -> (Rc<OperatorNode>, Vec<Rc<OperatorNode>>) {
    let agg = summary_agg(root);
    let Some(NonASAPOp::TimeRange { kind, child, .. }) = agg.children()[0].non_asap() else {
        panic!("expected a raw time range under the summary");
    };
    let panes: Vec<_> = (0..PANES)
        .map(|i| {
            let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms: i * PANE_MS,
                    at: None,
                },
                child: child.clone(),
            }))
            .unwrap();
            let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
                range: std::time::Duration::from_millis(PANE_MS as u64),
                kind: *kind,
                child: shifted,
            }))
            .unwrap();
            OperatorNode::new_shared(agg.operator.map_children(|_| range.clone())).unwrap()
        })
        .collect();
    let merged = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: panes.clone(),
    }))
    .unwrap();
    let root = replace(root, &agg, &merged);
    root.validate_structure().unwrap();
    (root, panes)
}

/// `root` with its summary built in tumbling panes by the planner's
/// window-composition rule, for a query repeating every minute: the
/// planned root (a `SummaryMerge` over one `PaneChain`) and its expansion
/// into the panes the executor runs.
fn planned_panes(root: &Rc<OperatorNode>) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
    let roots = [QueryRoot::Operator(Rc::clone(root))];
    let sites = pane_candidates(&roots, &[Some(PANE_MS as u64)]);
    let [site] = sites.as_slice() else {
        panic!("one build to split: {sites:?}")
    };
    assert_eq!((site.pane_width_ms, site.pane_count), (PANE_MS as u64, 5));
    let mut variants = window_composition_pass2_candidates(&roots, &sites).unwrap();
    let variant = variants.pop().unwrap();
    assert!(variants.is_empty());
    let QueryRoot::Operator(planned) = variant.roots.unwrap().remove(0) else {
        panic!("an operator root")
    };
    planned.validate_structure().unwrap();
    assert_eq!(
        planned.guarantee, root.guarantee,
        "merged panes keep the whole build's guarantee"
    );
    let expanded = expand_pane_chains(&planned).unwrap();
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

thread_local! {
    /// How far after each 15-s step the samples fall (0: on the pane
    /// boundaries).
    static PHASE_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// Two series, four positive samples per minute in [0, 5m), each
/// [`PHASE_MS`] after its 15-s step.
fn samples() -> Vec<(&'static str, i64, f64)> {
    let phase = PHASE_MS.with(|p| p.get());
    let mut rows = vec![];
    for (s, series) in ["a", "b"].into_iter().enumerate() {
        for minute in 0..PANES {
            for j in 0..4 {
                let ts = minute * PANE_MS + j * 15_000 + phase;
                rows.push((series, ts, (s * 100) as f64 + (minute * 4 + j + 1) as f64));
            }
        }
    }
    rows
}

struct Exported {
    dag: PhysicalASAPDAG,
    /// Each raw `TimeRange` input with the pane it reads (0 = most recent).
    ranges: Vec<(u64, i64)>,
    /// Each `SummaryAgg` node with its pane.
    builds: Vec<(u64, i64)>,
}

fn export(root: &Rc<OperatorNode>, assignment: &MaterializationAssignment) -> Exported {
    let timed = apply_materialization_timings(root, assignment, &mut TimingMemo::new()).unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    let pane = |node: &OperatorNode| -> i64 {
        match node.children()[0].non_asap() {
            Some(NonASAPOp::TimeShift { shift, .. }) => shift.offset_ms / PANE_MS,
            _ => 0,
        }
    };
    let (mut ranges, mut builds) = (vec![], vec![]);
    for node in &compiled.dag.nodes {
        let logical = compiled.node_ids.operator_node(node.id).unwrap();
        match &logical.operator {
            Operator::NonASAP(NonASAPOp::TimeRange { .. }) => {
                ranges.push((node.id as u64, pane(logical)))
            }
            Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => {
                builds.push((node.id as u64, pane(child)))
            }
            _ => {}
        }
    }
    Exported {
        dag: compiled.dag,
        ranges,
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

/// Raw rows for each `TimeRange` input. A deployment supplies pane `i` with
/// the samples in `[T - (i+1)·w, T - i·w)`; a single build reads all samples.
fn raw_inputs(exported: &Exported) -> BTreeMap<u64, Batch> {
    exported
        .ranges
        .iter()
        .map(|&(id, pane)| {
            let schema = schema_of(&exported.dag, id);
            let end = EVALUATION_MS - pane * PANE_MS;
            let start = if exported.ranges.len() == 1 {
                0
            } else {
                end - PANE_MS
            };
            let rows = samples()
                .into_iter()
                .filter(|(_, ts, _)| (start..end).contains(ts))
                .map(|(series, ts, value)| {
                    let labels = BTreeMap::from([
                        ("__name__".to_string(), "m".to_string()),
                        ("series".to_string(), series.to_string()),
                    ]);
                    promql_rows::series_row(&schema, &labels, ts, value).unwrap()
                })
                .collect();
            (id, Batch::try_new(schema, rows).unwrap())
        })
        .collect()
}

fn run(plan: &CompiledPhysicalDAG, inputs: BTreeMap<u64, Batch>, scope: Scope) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = physical_common::execute(plan, inputs, scope)
        .remove(0)
        .iter()
        .flat_map(|batch| batch.rows().to_vec())
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
        revision: 1,
    }
}

fn contracts(inputs: &BTreeMap<u64, Batch>) -> BTreeMap<u64, InputContract> {
    inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect()
}

/// Compile `root` with everything at query time (Pattern B2: panes rebuilt
/// from raw rows at query time).
fn compile_at_query_time(root: &Rc<OperatorNode>) -> Result<Exported, String> {
    let exported = export(root, &MaterializationAssignment::default());
    let inputs = raw_inputs(&exported);
    compile(
        &exported.dag,
        contracts(&inputs),
        &[exported.dag.roots[0] as u64],
    )
    .map_err(|e| e.to_string())?;
    Ok(exported)
}

fn execute(root: &Rc<OperatorNode>) -> Vec<Vec<Value>> {
    let exported = compile_at_query_time(root).unwrap();
    let inputs = raw_inputs(&exported);
    let plan = compile(
        &exported.dag,
        contracts(&inputs),
        &[exported.dag.roots[0] as u64],
    )
    .unwrap();
    run(&plan, inputs, query_scope())
}

/// Build the panes in ingestion runs (stored `MatAtIngestion`, each with its
/// own build timestamp), then merge them in the query run (Pattern B1).
fn execute_retained(root: &Rc<OperatorNode>, panes: &[Rc<OperatorNode>]) -> Vec<Vec<Value>> {
    let mut assignment = MaterializationAssignment::default();
    for pane in panes {
        assignment.set(pane, Materialization::MatAtIngestion(Medium::Memory));
    }
    execute_retained_as(root, &assignment)
}

/// The planner's pane chain maintained at ingestion time, as Stage 2's B1
/// option assigns it (`MatAtIngestion` on the chain), then expanded: each
/// pane carries the chain's timing and materialization, so the panes are
/// built in ingestion runs and merged in the query run.
fn execute_maintained_chain(planned: &Rc<OperatorNode>) -> Vec<Vec<Value>> {
    let chain = OperatorNode::reachable(planned)
        .into_iter()
        .find(|n| matches!(n.asap(), Some(ASAPOp::PaneChain { .. })))
        .expect("a pane chain");
    let mut assignment = MaterializationAssignment::default();
    assignment.set(&chain, Materialization::MatAtIngestion(Medium::Memory));
    let timed =
        apply_materialization_timings(planned, &assignment, &mut TimingMemo::new()).unwrap();
    let expanded = expand_pane_chains(&timed).unwrap();
    for pane in OperatorNode::reachable(&expanded)
        .iter()
        .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
    {
        assert_eq!(
            pane.materialization,
            Materialization::MatAtIngestion(Medium::Memory)
        );
    }
    execute_retained_as(&expanded, &MaterializationAssignment::default())
}

fn execute_retained_as(
    root: &Rc<OperatorNode>,
    assignment: &MaterializationAssignment,
) -> Vec<Vec<Value>> {
    let exported = export(root, assignment);
    let inputs = raw_inputs(&exported);
    let contracts = contracts(&inputs);
    let mut retained = BTreeMap::new();
    for &(build, pane) in &exported.builds {
        let range = exported.ranges.iter().find(|r| r.1 == pane).unwrap().0;
        let plan = compile(
            &exported.dag,
            BTreeMap::from([(range, contracts[&range].clone())]),
            &[build],
        )
        .unwrap();
        let end = EVALUATION_MS - pane * PANE_MS;
        let rows = run(
            &plan,
            BTreeMap::from([(range, inputs[&range].clone())]),
            Scope::Ingestion {
                window_start_ms: end - PANE_MS,
                window_end_ms: end,
                revision: 1,
            },
        );
        assert!(rows.iter().all(|row| has_timestamp(row, end)));
        retained.insert(
            build,
            Batch::try_new(schema_of(&exported.dag, build), rows).unwrap(),
        );
    }
    let plan = compile(
        &exported.dag,
        retained
            .iter()
            .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
            .collect(),
        &[exported.dag.roots[0] as u64],
    )
    .unwrap();
    run(&plan, retained, query_scope())
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
fn assert_pane_merge_matches_single_build(query: &str, accuracy: AccuracyTarget) -> Vec<f64> {
    let single = single_build(query, accuracy);
    let (panes, pane_builds) = tumbling_panes(&single);
    let expected = execute(&single);
    assert_eq!(expected.len(), 2, "{expected:?}");
    assert!(expected.iter().all(|row| has_timestamp(row, EVALUATION_MS)));
    let result = values(&expected);
    let expected = render(&expected);
    assert_eq!(
        render(&execute(&panes)),
        expected,
        "panes rebuilt at query time"
    );
    assert_eq!(
        render(&execute_retained(&panes, &pane_builds)),
        expected,
        "retained panes"
    );
    let (planned, expanded) = planned_panes(&single);
    assert_eq!(
        render(&execute(&expanded)),
        expected,
        "the planner's pane chain, expanded"
    );
    assert_eq!(
        render(&execute_maintained_chain(&planned)),
        expected,
        "the planner's pane chain, maintained at ingestion time"
    );
    let unexpanded = compile_at_query_time(&planned).err().unwrap_or_default();
    assert!(unexpanded.contains("PaneChain"), "{unexpanded}");
    result
}

/// Exact per-series Sum over 5 merged 1-minute panes equals one 5-minute
/// build, timestamped at the evaluation time.
#[test]
fn exact_sum_over_tumbling_panes_matches_single_build() {
    let sums =
        assert_pane_merge_matches_single_build("sum_over_time(m[5m])", AccuracyTarget::exact());
    assert_eq!(sums, vec![210., 2210.]);
}

/// The selected quantile sketch over 5 merged 1-minute panes equals one
/// 5-minute build, within the target of the exact p99 (20 and 120).
#[test]
fn quantile_over_tumbling_panes_matches_single_build() {
    let p99 = assert_pane_merge_matches_single_build(
        "quantile_over_time(0.99, m[5m])",
        AccuracyTarget::epsilon(0.05).with_metric(ErrorMetric::Relative),
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

/// Exact counter Rate over 5 merged 1-minute panes equals one 5-minute
/// build: the merged increase is divided by the whole 5-minute window, not
/// by one pane's minute.
#[test]
fn exact_rate_over_tumbling_panes_matches_single_build() {
    let rates = assert_pane_merge_matches_single_build("rate(m[5m])", AccuracyTarget::exact());
    assert_eq!(rates.len(), 2);
    // 19 counted over 285 s of samples, extrapolated to the 5-min window.
    for rate in rates {
        assert!((rate - 20.0 / 300.0).abs() < 1e-9, "{rate}");
    }
}

/// With samples off the pane boundaries, Increase extrapolates to the
/// start of the whole 5-minute window, as one build does, not to the start
/// of the newest pane.
#[test]
fn exact_increase_off_the_pane_boundaries_matches_single_build() {
    PHASE_MS.with(|p| p.set(7_000));
    let increases =
        assert_pane_merge_matches_single_build("increase(m[5m])", AccuracyTarget::exact());
    assert_eq!(increases.len(), 2);
}

/// The single build of `query`, evaluated at `evaluation_ms` over every
/// sample, as values.
fn whole_build_values_at(query: &str, evaluation_ms: i64) -> Vec<f64> {
    let root = single_build(query, AccuracyTarget::exact());
    let exported = export(&root, &MaterializationAssignment::default());
    let inputs: BTreeMap<u64, Batch> = exported
        .ranges
        .iter()
        .map(|&(id, _)| {
            let schema = schema_of(&exported.dag, id);
            let rows = samples()
                .into_iter()
                .map(|(series, ts, value)| {
                    let labels = BTreeMap::from([
                        ("__name__".to_string(), "m".to_string()),
                        ("series".to_string(), series.to_string()),
                    ]);
                    promql_rows::series_row(&schema, &labels, ts, value).unwrap()
                })
                .collect();
            (id, Batch::try_new(schema, rows).unwrap())
        })
        .collect();
    let plan = compile(
        &exported.dag,
        contracts(&inputs),
        &[exported.dag.roots[0] as u64],
    )
    .unwrap();
    values(&run(
        &plan,
        inputs,
        Scope::Query {
            evaluation_time_ms: evaluation_ms,
            revision: 1,
        },
    ))
}

/// A counter over `[5m] offset 1m` evaluated a minute later reads the same
/// window as one without the offset: the counter window ends at the
/// evaluation time minus the offset, not at the evaluation time.
#[test]
fn offset_counter_window_ends_at_the_offset() {
    PHASE_MS.with(|p| p.set(7_000));
    for function in ["rate", "increase"] {
        let plain = whole_build_values_at(&format!("{function}(m[5m])"), EVALUATION_MS);
        let shifted = whole_build_values_at(
            &format!("{function}(m[5m] offset 1m)"),
            EVALUATION_MS + PANE_MS,
        );
        assert_eq!(shifted, plain, "{function}");
    }
}

/// Exact counter Increase over 5 merged 1-minute panes equals one 5-minute
/// build: the merge joins consecutive panes across their boundaries.
#[test]
fn exact_increase_over_tumbling_panes_matches_single_build() {
    let increases =
        assert_pane_merge_matches_single_build("increase(m[5m])", AccuracyTarget::exact());
    assert_eq!(increases.len(), 2);
}
