//! Tumbling panes merged by `SummaryMerge` answer the same per-series query as
//! one build over the whole window (#509 Example 3/4, Pattern B; #580).
use asap_executor::{
    operators::Operator as PhysicalOperator,
    physical_planner::{
        compile, promql_rows::encode_series_identity, CompiledPhysicalDAG, InputContract, Source,
    },
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use asap_logical_optimizer::search_workload;
use asap_plan_selection::candidate_selection::global_selection;
use asap_plan_selection::cost::cost_model::DefaultCostModel;
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::operator_properties::TimeShift;
use planner_types::ir::physical_export::compile_physical_asap_dag_with_node_ids;
use planner_types::ir::physical_export::PhysicalASAPDAG;
use planner_types::ir::{properties::ExecutionTiming, ASAPOp, NonASAPOp, Operator, OperatorNode};
use planner_types::types::AccuracyTarget;
use planner_types::workload::*;
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

const EVALUATION_MS: i64 = 300_000;
const PANE_MS: i64 = 60_000;
const PANES: i64 = 5;

/// The selected single-build plan for a 5-minute per-series PromQL query.
fn single_build(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let root = asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0);
    let root = asap_executor::physical_planner::promql_rows::with_series_identity(&root).unwrap();
    let space = search_workload(vec![("q", root)]);
    global_selection(&space, &DefaultCostModel)
        .assemble_selected_query(&space.roots[0].1)
        .unwrap()
        .unwrap()
}

/// Rewrite the root's per-entity `SummaryAgg` over `TimeRange(5m)` into the
/// shape #580 emits: pane `i` is `TimeRange(1m)` over `TimeShift(i·1m)` over
/// the same `Scan`, and a `SummaryMerge` combines the panes. Each pane's
/// coverage is derived from its `TimeRange` over `TimeShift`.
fn tumbling_panes(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    let [agg] = root.operator.children()[..] else {
        panic!("expected one summary input");
    };
    let Some(NonASAPOp::TimeRange { kind, child, .. }) = agg.operator.children()[0].non_asap()
    else {
        panic!("expected a raw time range under the summary");
    };
    let panes = (0..PANES)
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
            Rc::new(
                agg.with_new_children(|_| range.clone())
                    .unwrap()
                    .with_guarantee(agg.guarantee.clone()),
            )
        })
        .collect();
    let merged =
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children: panes })).unwrap();
    let merged = Rc::new((*merged).clone().with_guarantee(agg.guarantee.clone()));
    let root = Rc::new(
        root.with_new_children(|_| merged.clone())
            .unwrap()
            .with_guarantee(root.guarantee.clone()),
    );
    root.validate_structure().unwrap();
    root
}

/// Two series, four samples per minute in [0, 5m): row `(series, minute, j)`.
fn samples() -> Vec<(&'static str, i64, f64)> {
    let mut rows = vec![];
    for (s, series) in ["a", "b"].into_iter().enumerate() {
        for minute in 0..PANES {
            for j in 0..4 {
                let ts = minute * PANE_MS + j * 15_000;
                rows.push((series, ts, (s * 100) as f64 + (minute * 4 + j) as f64));
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

/// Place every node at query time. `apply_materialization_timings` does not
/// yet accept `SummaryMerge` (it reports `UnimplementedOperator`), so this test
/// assigns the timing a planner would choose for Pattern B2 directly.
fn at_query_time(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    let mut timed = (**node).clone();
    timed.operator = node.operator.map_children(at_query_time);
    timed.timing = Some(ExecutionTiming::QueryTime);
    Rc::new(timed)
}

fn export(root: &Rc<OperatorNode>) -> Exported {
    let root = at_query_time(root);
    let compiled = compile_physical_asap_dag_with_node_ids(&root).unwrap();
    let pane = |node: &OperatorNode| -> i64 {
        match node.operator.children()[0].non_asap() {
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

fn schema_of(dag: &PhysicalASAPDAG, id: u64) -> Arc<planner_types::ir::schema::Schema> {
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
                    schema
                        .fields
                        .iter()
                        .map(|field| match field.name.as_str() {
                            "ts" => Value::Timestamp(ts),
                            "value" => Value::Float64(value),
                            _ => Value::Utf8(
                                encode_series_identity(&BTreeMap::from([
                                    ("__name__".to_string(), "m".to_string()),
                                    ("series".to_string(), series.to_string()),
                                ]))
                                .unwrap()
                                .into(),
                            ),
                        })
                        .collect()
                })
                .collect();
            (id, Batch::try_new(schema, rows).unwrap())
        })
        .collect()
}

fn run(plan: &CompiledPhysicalDAG, inputs: BTreeMap<u64, Batch>, scope: Scope) -> Vec<Vec<Value>> {
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            let source = PhysicalOperator::source(batch.schema().clone(), vec![batch]).unwrap();
            (id, Box::new(source) as Source<'_>)
        })
        .collect();
    let dag = plan.instantiate(sources).unwrap();
    let mut rows = block_on(async {
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let mut output = dag.execute(plan.roots(), context).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = output.next().await {
            rows.extend(batch.unwrap().rows().iter().cloned());
        }
        rows
    });
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

/// Compile and run `root` with every raw time range as a deployment input.
fn execute(root: &Rc<OperatorNode>) -> Vec<Vec<Value>> {
    let exported = export(root);
    let inputs = raw_inputs(&exported);
    let contracts = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
    let plan = compile(&exported.dag, contracts, &[exported.dag.roots[0] as u64]).unwrap();
    run(&plan, inputs, query_scope())
}

/// Build panes in an ingestion run, retain them with distinct build
/// timestamps, then merge them in the query run (Pattern B1).
fn execute_retained(root: &Rc<OperatorNode>) -> Vec<Vec<Value>> {
    let exported = export(root);
    let inputs = raw_inputs(&exported);
    let contracts: BTreeMap<_, _> = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
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
    let panes = tumbling_panes(&single);
    let expected = execute(&single);
    assert_eq!(expected.len(), 2, "{expected:?}");
    // The single build already carries the evaluation timestamp.
    assert!(expected.iter().all(|row| has_timestamp(row, EVALUATION_MS)));
    let result = values(&expected);
    let expected = render(&expected);
    assert_eq!(
        render(&execute(&panes)),
        expected,
        "panes rebuilt at query time"
    );
    assert_eq!(
        render(&execute_retained(&panes)),
        expected,
        "retained panes"
    );
    result
}

/// Exact per-series Sum over 5 merged 1-minute panes equals one 5-minute build,
/// timestamped at the evaluation time.
#[test]
fn exact_sum_over_tumbling_panes_matches_single_build() {
    let sums =
        assert_pane_merge_matches_single_build("sum_over_time(m[5m])", AccuracyTarget::Exact);
    assert_eq!(sums, vec![190., 2190.]);
}

/// A KLL quantile over 5 merged 1-minute panes equals one 5-minute build. The
/// 20 samples per series are below k, so both states are exact.
#[test]
fn kll_quantile_over_tumbling_panes_matches_single_build() {
    let p99 = assert_pane_merge_matches_single_build(
        "quantile_over_time(0.99, m[5m])",
        AccuracyTarget::Epsilon(0.05),
    );
    assert_eq!(p99, vec![19., 119.]);
}
