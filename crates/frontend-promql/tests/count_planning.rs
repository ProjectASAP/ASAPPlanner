//! Query text through summary selection: counts use observations, never value weights.
use crate::planner_support::selected_dag;
use crate::support;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::summary_type::ExactKind;
use support::lower_promql;

// Exact series and temporal counts must select a count accumulator, not distinct or sum.
#[test]
fn exact_counts_select_count_accumulators() {
    for query in ["count(up)", "count by(job)(up)", "count_over_time(up[5m])"] {
        let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
        let selected = selected_dag(root, AccuracyTarget::exact());
        assert!(
            OperatorNode::reachable(&selected).iter().any(|node| {
                matches!(
                    &node.operator,
                    Operator::ASAP(ASAPOp::SummaryAgg {
                        summary_type: FieldDataType::ExactAggregate(ExactKind::Count, _),
                        ..
                    })
                )
            }),
            "{query}: {selected:?}"
        );
    }
}

// Fixtures are already selected at one instant or within one five-minute window.
// This narrow test oracle interprets the emitted aggregate, not Prometheus ingestion,
// staleness, or scrape scheduling. Unsupported plan shapes fail explicitly.
fn aggregate_fixture(query: &str, series: &[Vec<f64>]) -> Vec<f64> {
    use asap_types::ir::operator::{AggIntent, Reduction};
    use asap_types::ir::NonASAPOp;
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
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
        [AggIntent::Count] => values.len() as f64,
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

// The assertion helper must fail on an injected composition error; its coverage
// must not depend on a real planner bug remaining unfixed.
#[test]
#[should_panic(expected = "does not compose")]
fn logical_candidates_fails_on_a_candidate_that_does_not_compose() {
    support::require_composed_operator_candidates(vec![support::LogicalCandidate {
        id: "invalid-composition".into(),
        roots: Err("invalid choice".into()),
    }]);
}
