//! Query text through summary selection: counts use observations, never value weights.
use std::rc::Rc;

use asap_aware_mapping::accuracy::DefaultAccuracyModel;
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::{
    default_strategies, search_workload_with_targets, Replacement, ReplacementStrategy,
    SketchAlgorithmStrategy, TargetSubDAG,
};
mod support;
use asap_types::post_asap::{
    compile_executable_dag, ExactKind, ExecutableOperatorPayload, NonNegativeWeightProof,
    SketchAlgorithm, SummaryExpr, SummaryFamilyType, SummaryInputExpr, WeightDomain,
};
use asap_types::types::AccuracyTarget;
use support::lower_promql;

#[test]
fn grouped_count_keeps_uncertified_hydra_candidates_for_backend_review() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = Rc::new(lower_promql("count by(job)(up)", target.clone()).unwrap());
    let space = search_workload_with_targets(
        vec![("count", root, Some(target))],
        &default_strategies(),
        &DefaultAccuracyModel,
    );
    let planned = &space.roots[0].1;
    let hydra: Vec<_> = space
        .candidates_for_target(planned)
        .unwrap()
        .candidates
        .iter()
        .filter(|candidate| candidate.strategy == "HydraGroupingStrategy")
        .collect();
    assert_eq!(hydra.len(), 2);
    assert!(hydra
        .iter()
        .all(|candidate| candidate.has_missing_accuracy_evidence()));
    assert!(!space
        .global_selection(&DefaultCostModel)
        .for_target(planned)
        .unwrap()
        .chosen
        .is_some_and(|candidate| candidate.has_missing_accuracy_evidence()));
}

// Exact series and temporal counts must select a count accumulator, not distinct or sum.
#[test]
fn exact_counts_select_count_accumulators() {
    for query in ["count(up)", "count by(job)(up)", "count_over_time(up[5m])"] {
        let root = Rc::new(lower_promql(query, AccuracyTarget::Exact).unwrap());
        let candidates =
            SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
        assert!(
            candidates.iter().any(|candidate| {
                matches!(&candidate.replacement, Replacement::Summary(node)
                if matches!(&node.expr, SummaryExpr::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Count, _), .. }))
            }),
            "{query}: {candidates:?}"
        );
    }
}

// CMS/CountSketch count updates must stay +1 even for zero or negative samples.
#[test]
fn frequency_count_candidates_use_unit_weights() {
    for query in ["count_over_time(up[5m])", "count(up)"] {
        let root = Rc::new(lower_promql(query, AccuracyTarget::Epsilon(0.02)).unwrap());
        let candidates =
            SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
        let mut algorithms = Vec::new();
        for candidate in &candidates {
            let Replacement::Summary(node) = &candidate.replacement else {
                continue;
            };
            let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
                continue;
            };
            let SummaryExpr::SummaryAgg {
                family: SummaryFamilyType::Sketch(kind, _),
                input,
                ..
            } = &summary_input.expr
            else {
                continue;
            };
            assert!(
                !matches!(kind.algorithm(), SketchAlgorithm::Hll),
                "{query}: {node:?}"
            );
            if !matches!(
                kind.algorithm(),
                SketchAlgorithm::Cms | SketchAlgorithm::CountSketch
            ) {
                continue;
            }
            let dag = compile_executable_dag(node).unwrap();
            assert!(
                dag.nodes.iter().any(|node| matches!(
                    &node.payload,
                    ExecutableOperatorPayload::SummaryAgg { input: actual, .. } if actual == input
                )),
                "executable DAG must preserve the count update contract"
            );
            algorithms.push(kind.algorithm().clone());
            assert!(input.item.is_some(), "frequency keys must be explicit");
            assert_eq!(
                input.weight,
                SummaryInputExpr::Constant(1.0),
                "{query}: {node:?}"
            );
            assert_eq!(
                input.weight_domain,
                WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount
                }
            );
        }
        assert!(
            algorithms.contains(&SketchAlgorithm::Cms),
            "{query}: {candidates:?}"
        );
        assert!(
            algorithms.contains(&SketchAlgorithm::CountSketch),
            "{query}: {candidates:?}"
        );
    }
}

// Fixtures are already selected at one instant or within one five-minute window.
// This narrow test oracle interprets the emitted aggregate, not Prometheus ingestion,
// staleness, or scrape scheduling. Unsupported plan shapes fail explicitly.
fn aggregate_fixture(query: &str, series: &[Vec<f64>]) -> Vec<f64> {
    use asap_types::pre_asap::{AggIntent, QueryExpr, Reduction};
    let root = lower_promql(query, AccuracyTarget::Exact).unwrap();
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = &root
    else {
        panic!("expected aggregate: {root:?}");
    };
    match child.as_ref() {
        QueryExpr::Scan { .. } => assert!(series.iter().all(|samples| samples.len() == 1)),
        QueryExpr::TimeRange { range, child } => {
            assert!(matches!(range.as_secs(), 1 | 300));
            if range.as_secs() == 1 {
                assert!(series.iter().all(|samples| samples.len() == 1));
            }
            assert!(matches!(child.as_ref(), QueryExpr::Scan { .. }));
        }
        other => panic!("unsupported fixture input: {other:?}"),
    }
    let aggregate = |values: &[f64]| match measures.as_slice() {
        [AggIntent::Count { .. }] => values.len() as f64,
        [AggIntent::Cardinality { .. }] => {
            let mut distinct = values.to_vec();
            distinct.sort_by(f64::total_cmp);
            distinct.dedup();
            distinct.len() as f64
        }
        [AggIntent::Sum { .. }] => values.iter().sum(),
        other => panic!("unsupported fixture aggregate: {other:?}"),
    };
    match reduction {
        Reduction::PerEntity => series.iter().map(|values| aggregate(values)).collect(),
        Reduction::Reduce(_) => {
            assert_eq!(reduction, &Reduction::by(vec![]));
            vec![aggregate(
                &series.iter().flatten().copied().collect::<Vec<_>>(),
            )]
        }
    }
}

// Three targets remain three whether healthy, unhealthy, or carrying signed values.
#[test]
fn count_up_is_three_independent_of_target_health() {
    for values in [[1.0, 1.0, 1.0], [1.0, 1.0, 0.0], [-1.0, -1.0, 0.0]] {
        let series: Vec<_> = values.into_iter().map(|value| vec![value]).collect();
        assert_eq!(
            aggregate_fixture("count(up)", &series),
            vec![3.0],
            "{values:?}"
        );
    }
}

// Ten scrapes per series count as ten, including all-zero and all-negative series.
#[test]
fn count_over_time_counts_scrapes_not_sample_values() {
    // up{instance="a"} and up{instance="b"}, evaluated in the same window.
    assert_eq!(
        aggregate_fixture("count_over_time(up[5m])", &[vec![1.0; 10], vec![0.0; 10]]),
        vec![10.0, 10.0]
    );
    // The http_reqs series is a separate metric and therefore a separate query.
    assert_eq!(
        aggregate_fixture(
            "count_over_time(http_reqs{code=\"200\"}[5m])",
            &[vec![3.0; 10]]
        ),
        vec![10.0]
    );
    assert_eq!(
        aggregate_fixture(
            "count_over_time(temperature[5m])",
            &[
                vec![-3.0; 10],
                vec![-2.0, 0.0, 2.0, -2.0, 0.0, 2.0, -2.0, 0.0, 2.0, -2.0]
            ]
        ),
        vec![10.0, 10.0]
    );
}

// Execute the emitted CMS update-weight expression on the reported values.
// This checks the planner's numerical update contract, not a sketch-library runtime.
#[test]
fn cms_count_updates_total_ten_for_zero_positive_and_negative_samples() {
    use asap_types::pre_asap::ColumnRef;
    let root =
        Rc::new(lower_promql("count_over_time(up[5m])", AccuracyTarget::Epsilon(0.02)).unwrap());
    let candidates =
        SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
    let dag = candidates
        .iter()
        .find_map(|candidate| {
            let Replacement::Summary(node) = &candidate.replacement else {
                return None;
            };
            let dag = compile_executable_dag(node).unwrap();
            dag.nodes
                .iter()
                .any(|node| {
                    matches!(&node.payload,
            ExecutableOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. }
                if kind.algorithm() == &SketchAlgorithm::Cms)
                })
                .then_some(dag)
        })
        .expect("CMS count candidate");
    let update = dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            ExecutableOperatorPayload::SummaryAgg { input, .. } => Some(input),
            _ => None,
        })
        .unwrap();
    for value in [1.0, 0.0, 3.0, -3.0] {
        let total: f64 = [value; 10]
            .into_iter()
            .map(|sample| {
                let weight = match &update.weight {
                    SummaryInputExpr::Constant(value) => *value,
                    SummaryInputExpr::Column(ColumnRef::SampleValue) => sample,
                    SummaryInputExpr::Column(ColumnRef::Named(name)) if name == "value" => sample,
                    other => panic!("unsupported update weight: {other:?}"),
                };
                assert!(
                    weight >= 0.0,
                    "CMS must not receive a negative update for {sample}"
                );
                weight
            })
            .sum();
        assert_eq!(total, 10.0, "ten samples of {value}");
    }
}
