//! Exact per-series range reductions over raw samples execute natively
//! (issue #770): exact `quantile/sum/avg/min/max/count_over_time` reduces
//! each series' samples in the window with a per-entity `Aggregate`, which
//! the executor compiles and runs.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::physical_planner::{
    compile_candidate, frontier_from_timing, promql_rows, InputContract,
};
use asap_executor::runtime::Scope;
use asap_executor::series_labels_of_output_row;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::ir::operator::Reduction;
use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{
    apply_materialization_timings, MaterializationAssignment, NonASAPOp, OperatorNode, TimingMemo,
};

mod physical_common;

const EVALUATION_MS: i64 = 300_000;
/// Raw `x` samples per series `instance`, all inside the last minute.
const SAMPLES: [(&str, &[f64]); 2] = [("a", &[1.0, 4.0, 2.0, 7.0]), ("b", &[10.0, 30.0, 20.0])];

/// `query` lowered exactly, with series identity: a per-entity `Aggregate`
/// over the window's raw samples, as the exact plan that reduces raw samples
/// runs it.
fn per_entity_plan(query: &str) -> Rc<OperatorNode> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap();
    assert!(
        matches!(
            root.non_asap(),
            Some(NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                ..
            })
        ),
        "{query}: {:?}",
        root.operator
    );
    root
}

/// Execute `plan` at query time over `SAMPLES`: each series'
/// `instance` and its value.
fn execute(plan: &Rc<OperatorNode>) -> Vec<(String, f64)> {
    let timed = apply_materialization_timings(
        plan,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let [range] = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::TimeRange { .. })
            )
        })
        .collect::<Vec<_>>()[..]
    else {
        panic!("one range selector");
    };
    let schema = Arc::new(range.output_schema.clone());
    let compiled = compile_candidate(
        &dag,
        BTreeMap::from([(range.id as u64, InputContract::bounded(schema.clone()))]),
        &[dag.roots[0] as u64],
        &frontier_from_timing(&dag).unwrap(),
    )
    .unwrap();
    let rows = SAMPLES
        .iter()
        .flat_map(|(instance, values)| {
            let labels = BTreeMap::from([
                ("__name__".to_string(), "x".to_string()),
                ("instance".to_string(), instance.to_string()),
            ]);
            let schema = schema.clone();
            values.iter().enumerate().map(move |(i, value)| {
                promql_rows::series_row(&schema, &labels, 250_000 + 10_000 * i as i64, *value)
                    .unwrap()
            })
        })
        .collect();
    let input = Batch::try_new(schema.clone(), rows).unwrap();
    let result = physical_common::execute(
        &compiled.query,
        BTreeMap::from([(range.id as u64, input)]),
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
    );
    let mut answers: Vec<(String, f64)> = result[0]
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let labels = series_labels_of_output_row(batch.schema(), &row).unwrap();
                assert!(!labels.contains_key("__name__"), "{labels:?}");
                let value = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (labels["instance"].clone(), value.expect("a Float64 answer"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

// Each range function's exact per-entity plan compiles and gives, per
// series, the function of that series' window samples, without the metric
// name. Series `a` holds 1, 4, 2, 7 and `b` holds 10, 30, 20.
#[test]
fn exact_per_series_range_functions_execute() {
    for (query, a, b) in [
        ("sum_over_time(x[1m])", 14.0, 60.0),
        ("avg_over_time(x[1m])", 3.5, 20.0),
        ("min_over_time(x[1m])", 1.0, 10.0),
        ("max_over_time(x[1m])", 7.0, 30.0),
        ("count_over_time(x[1m])", 4.0, 3.0),
        // Ranks interpolate: a's median sits halfway between 2 and 4.
        ("quantile_over_time(0.5, x[1m])", 3.0, 20.0),
    ] {
        let answers = execute(&per_entity_plan(query));
        assert_eq!(
            answers,
            [("a".to_string(), a), ("b".to_string(), b)],
            "{query}"
        );
    }
}
