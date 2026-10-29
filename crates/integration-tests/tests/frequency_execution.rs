//! #509 Example 2's PromQL stand-in executes natively: the distinct count,
//! entropy and L2 norm of one series' sample values over 1 min, through the
//! exact candidate and through the one that builds a UnivMon, at query time.

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::Value;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::explicit_local_candidates;
use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NodeToTimedNodeCache,
    NonASAPOp, OperatorNode,
};

use crate::physical_common;

const EVALUATION_MS: i64 = 300_000;
/// `flows_src_ip` samples: source 1 twice, 2 three times, 3 once.
const SAMPLES: [f64; 6] = [1.0, 2.0, 1.0, 2.0, 3.0, 2.0];

/// The summary family a candidate builds: `None` for the exact plan.
fn built_family(candidate: &Rc<OperatorNode>) -> Option<FieldDataType> {
    OperatorNode::reachable(candidate)
        .iter()
        .find_map(|node| match node.asap() {
            Some(ASAPOp::SummaryAgg { family, .. }) => Some(family.clone()),
            _ => None,
        })
}

/// Execute `candidate` at query time over `SAMPLES`, all inside the window.
fn execute(candidate: &Rc<OperatorNode>) -> Vec<f64> {
    let timed = apply_materialization_timings(
        candidate,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let ranges = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::TimeRange { .. })
            )
        })
        .count();
    assert_eq!(ranges, 1, "one range selector");
    // The whole plan runs at query time over the raw samples.
    let compiled = compile(&dag, &[], &[dag.roots[0] as u64]).unwrap();
    let labels = BTreeMap::from([("__name__".to_string(), "flows_src_ip".to_string())]);
    let store = physical_common::raw_data_store(&dag, |_, schema| {
        SAMPLES
            .iter()
            .enumerate()
            .map(|(i, value)| {
                promql_rows::series_row(schema, &labels, 290_000 + i as i64, *value).unwrap()
            })
            .collect()
    });
    let result = physical_common::execute(
        &compiled,
        store,
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        },
    );
    result[0]
        .iter()
        .flat_map(|batch| batch.to_rows())
        .flatten()
        .filter_map(|value| match value {
            Value::Float64(value) => Some(value),
            Value::Int64(value) => Some(value as f64),
            _ => None,
        })
        .collect()
}

// The exact candidate and the UnivMon candidate of each statistic both
// execute and give the statistic of the window's sample values (entropy in
// bits).
#[test]
fn standin_exact_and_univmon_candidates_execute() {
    let entropy = -[2.0_f64, 3.0, 1.0]
        .iter()
        .map(|count| count / 6.0 * (count / 6.0).log2())
        .sum::<f64>();
    // A UnivMon sized for a distinct count at ε = 0.01 holds a heap of
    // millions of keys on each of its 19 layers, past the default 64 MiB.
    for (query, expected, epsilon, delta) in [
        ("distinct_over_time(flows_src_ip[1m])", 3.0, 0.2, 0.1),
        ("entropy_over_time(flows_src_ip[1m])", entropy, 0.01, 0.01),
        (
            "l2_over_time(flows_src_ip[1m])",
            14.0_f64.sqrt(),
            0.01,
            0.01,
        ),
    ] {
        let accuracy = AccuracyTarget::max_error(epsilon).failure_probability(delta);
        let root = lower_promql(query, accuracy.clone()).unwrap();
        let root = promql_rows::with_series_identity(&root).unwrap();
        let mut executed = Vec::new();
        // This is an operator execution test. Enumerate each local rule once,
        // not an arbitrary sized configuration visited by a budgeted search.
        for candidate in explicit_local_candidates(root) {
            let family = built_family(&candidate);
            let name = match &family {
                None => "exact".to_string(),
                Some(FieldDataType::Sketch(kind, _))
                    if kind.algorithm() == &SketchAlgorithm::UnivMon =>
                {
                    "UnivMon".to_string()
                }
                _ => continue,
            };
            if query.starts_with("distinct_over_time") {
                if let Some(FieldDataType::Sketch(kind, _)) = &family {
                    let asap_types::ir::schema::SketchParams::UnivMon {
                        distinct_failure_probability,
                        ..
                    } = kind.params()
                    else {
                        unreachable!("family filtered above")
                    };
                    assert_eq!(*distinct_failure_probability, Some(delta));
                }
            }
            let values = execute(&candidate);
            assert!(
                matches!(values.as_slice(), [value] if (value - expected).abs() < 1e-9),
                "{query} {name}: {values:?}, expected {expected}"
            );
            executed.push(name);
        }
        executed.sort();
        assert_eq!(executed, ["UnivMon", "exact"], "{query}");
    }
}
