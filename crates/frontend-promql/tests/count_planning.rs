//! Query text through summary selection: counts use observations, never value weights.
mod support;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{
    ExactKind, FieldDataType, GroupingStrategy, NonNegativeWeightProof, SketchAlgorithm,
    SummaryInputExpr, WeightDomain,
};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;
use support::{logical_candidates, lower_promql, post_asap_dag, selected_dag};

/// Whether `node` reads a Hydra state (one sketch shared by every group).
fn reads_hydra(node: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(node).iter().any(|node| {
        matches!(
            &node.operator,
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { .. }),
                ..
            })
        )
    })
}

#[test]
#[ignore = "Pass 1 offers no Hydra state for a grouped count yet (#605)"]
fn grouped_count_offers_hydra_but_selection_keeps_per_group_state() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    // Hydra hashes a non-null item per row: the series identity (PromQL
    // labels are nullable).
    let root = asap_types::ir::schema_support::with_promql_series_identity(
        &lower_promql("count by(job)(up)", target.clone()).unwrap(),
    )
    .unwrap();
    assert!(logical_candidates(Rc::clone(&root), target.clone())
        .iter()
        .any(reads_hydra));
    assert!(!reads_hydra(&selected_dag(root, target)));
}

// Exact series and temporal counts must select a count accumulator, not distinct or sum.
#[test]
fn exact_counts_select_count_accumulators() {
    for query in ["count(up)", "count by(job)(up)", "count_over_time(up[5m])"] {
        let root = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let selected = selected_dag(root, AccuracyTarget::Exact);
        assert!(
            OperatorNode::reachable(&selected).iter().any(|node| {
                matches!(
                    &node.operator,
                    Operator::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::ExactAggregate(ExactKind::Count, _),
                        ..
                    })
                )
            }),
            "{query}: {selected:?}"
        );
    }
}

// CMS/CountSketch count updates must stay +1 even for zero or negative samples.
#[test]
fn frequency_count_candidates_use_unit_weights() {
    for query in ["count_over_time(up[5m])", "count(up)"] {
        let accuracy = AccuracyTarget::Epsilon(0.02);
        let root = lower_promql(query, accuracy.clone()).unwrap();
        let candidates = logical_candidates(root, accuracy);
        let mut algorithms = Vec::new();
        for node in &candidates {
            let Some(summary_input) =
                OperatorNode::reachable(node)
                    .into_iter()
                    .find_map(|node| match &node.operator {
                        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                            Some(Rc::clone(summary_input))
                        }
                        _ => None,
                    })
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
    use asap_types::ir::NonASAPOp;
    use asap_types::pre_asap::{AggIntent, Reduction};
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
    use asap_types::pre_asap::ColumnRef;
    let accuracy = AccuracyTarget::Epsilon(0.02);
    let root = lower_promql("count_over_time(up[5m])", accuracy.clone()).unwrap();
    let candidates = logical_candidates(root, accuracy);
    let dag = candidates
        .iter()
        .find_map(|node| {
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

/// A candidate that does not compose fails `logical_candidates` instead of
/// being skipped, so an assertion over every candidate cannot pass on none.
/// An aggregate over a top-k output does not compose yet (#672).
/// This test relies on that bug: when #672 is fixed, the query composes and
/// this test must switch to another candidate that does not compose.
#[test]
#[should_panic(expected = "does not compose")]
fn logical_candidates_fails_on_a_candidate_that_does_not_compose() {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let root = lower_promql(
        "sum by (job) (topk by (job, instance) (1, rate(m[5m])))",
        accuracy.clone(),
    )
    .unwrap();
    logical_candidates(root, accuracy);
}
