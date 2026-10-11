//! Query text through summary selection: counts use observations, never value weights.
use asap_logical_optimizer::accuracy::DefaultAccuracyModel;
use asap_logical_optimizer::{
    default_strategies, search_workload_with_targets, ASAPStrategies, Replacement,
    ReplacementStrategy, TargetSubDAG,
};
use asap_plan_selection::candidate_selection::global_selection;
use asap_plan_selection::cost::cost_model::DefaultCostModel;
mod support;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::schema::{
    ExactKind, FieldDataType, NonNegativeWeightProof, SketchAlgorithm, SummaryInputExpr,
    WeightDomain,
};
use asap_types::ir::{ASAPOp, Operator};
use asap_types::types::AccuracyTarget;
use support::{lower_promql, post_asap_dag};

#[test]
fn grouped_count_keeps_uncertified_hydra_candidates_for_backend_review() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_promql("count by(job)(up)", target.clone()).unwrap();
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
    assert!(!global_selection(&space, &DefaultCostModel)
        .for_target(planned)
        .unwrap()
        .chosen
        .is_some_and(|candidate| candidate.has_missing_accuracy_evidence()));
}

// Exact series and temporal counts must select a count accumulator, not distinct or sum.
#[test]
fn exact_counts_select_count_accumulators() {
    for query in ["count(up)", "count by(job)(up)", "count_over_time(up[5m])"] {
        let root = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
        assert!(
            candidates.iter().any(|candidate| {
                matches!(&candidate.replacement, Replacement::SubDAG(node)
                if matches!(&node.operator, Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Count, _), .. })))
            }),
            "{query}: {candidates:?}"
        );
    }
}

// CMS/CountSketch count updates must stay +1 even for zero or negative samples.
#[test]
fn frequency_count_candidates_use_unit_weights() {
    for query in ["count_over_time(up[5m])", "count(up)"] {
        let root = lower_promql(query, AccuracyTarget::Epsilon(0.02)).unwrap();
        let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
        let mut algorithms = Vec::new();
        for candidate in &candidates {
            let Replacement::SubDAG(node) = &candidate.replacement else {
                continue;
            };
            let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator
            else {
                continue;
            };
            let Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                input,
                ..
            }) = &summary_input.operator
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
            let dag = post_asap_dag(node);
            assert!(
                dag.nodes.iter().any(|node| matches!(
                    &node.payload,
                    PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { input: actual, .. }) if actual == input
                )),
                "post-ASAP DAG must preserve the count update contract"
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
    use asap_types::ir::operator::{AggIntent, Reduction};
    use asap_types::ir::NonASAPOp;
    let root = lower_promql(query, AccuracyTarget::Exact).unwrap();
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = root.expect_non_asap()
    else {
        panic!("expected aggregate: {root:?}");
    };
    match child.expect_non_asap() {
        NonASAPOp::Scan { .. } => assert!(series.iter().all(|samples| samples.len() == 1)),
        NonASAPOp::TimeRange { range, child, .. } => {
            assert!(matches!(range.as_secs(), 1 | 300));
            if range.as_secs() == 1 {
                assert!(series.iter().all(|samples| samples.len() == 1));
            }
            assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
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
    use asap_types::ir::scalar::ColumnRef;
    let root = lower_promql("count_over_time(up[5m])", AccuracyTarget::Epsilon(0.02)).unwrap();
    let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
    let dag = candidates
        .iter()
        .find_map(|candidate| {
            let Replacement::SubDAG(node) = &candidate.replacement else {
                return None;
            };
            let dag = post_asap_dag(node);
            dag.nodes
                .iter()
                .any(|node| {
                    matches!(&node.payload,
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                if kind.algorithm() == &SketchAlgorithm::Cms)
                })
                .then_some(dag)
        })
        .expect("CMS count candidate");
    let update = dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { input, .. }) => Some(input),
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
