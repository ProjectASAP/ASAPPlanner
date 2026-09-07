use asap_aware_mapping::analytical_cost::{
    EvidenceBackedPhysicalDag, ExecutionMultiplicity, PhysicalDagNode, PhysicalNodeEvidence,
    PhysicalOperator,
};
use asap_aware_mapping::physical_operator_statistics::{
    ComparisonScope, EdgeStatistics, OperatorStatistics, SourceCoverage, UnaryEdgeStatistics,
};
use asap_types::pre_asap::query_expr::Source;
use asap_types::workload::{
    DataArrival, DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
};
use std::collections::HashMap;

fn fixture() -> (EvidenceBackedPhysicalDag, ComparisonScope) {
    let coverage = SourceCoverage {
        source: Source::Table {
            table_ref: "events".into(),
        },
        source_snapshot_id: "snapshot".into(),
        predicates: vec![],
        info_matchers: vec![],
    };
    let scope = ComparisonScope {
        data_arrival: DataArrival::AtRest,
        planning_time: TimestampMs(100),
        horizon: DurationMs(1000),
        recurrence: QueryRecurrence::OneTime {
            invocations: 3,
            execute_at: None,
        },
        time_selection: TimeSelection {
            scope: QueryTimeScope::Longitudinal,
            lookback: Some(DurationMs(1000)),
            as_of: Some(TimestampMs(100)),
        },
        sources: vec![coverage.clone()],
    };
    let edge = EdgeStatistics {
        rows: 10,
        bytes: 80,
    };
    let unary = UnaryEdgeStatistics {
        input: edge,
        output: edge,
        promql: None,
    };
    let nodes = vec![
        PhysicalDagNode {
            id: "scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(coverage),
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        },
        PhysicalDagNode {
            id: "left".into(),
            operator: PhysicalOperator::PassThrough,
            children: vec!["scan".into()],
            source_coverage: None,
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        },
        PhysicalDagNode {
            id: "right".into(),
            operator: PhysicalOperator::PassThrough,
            children: vec!["scan".into()],
            source_coverage: None,
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        },
        PhysicalDagNode {
            id: "root".into(),
            operator: PhysicalOperator::Concat,
            children: vec!["left".into(), "right".into()],
            source_coverage: None,
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::PerEvaluation,
        },
    ];
    let evidence = HashMap::from([
        (
            "scan".into(),
            PhysicalNodeEvidence {
                physical_id: "scan".into(),
                statistics: OperatorStatistics::Scan {
                    edges: unary,
                    source_read_bytes: 80,
                },
                output_buffer_bytes: 0,
            },
        ),
        (
            "left".into(),
            PhysicalNodeEvidence {
                physical_id: "left".into(),
                statistics: OperatorStatistics::PassThrough { edges: unary },
                output_buffer_bytes: 0,
            },
        ),
        (
            "right".into(),
            PhysicalNodeEvidence {
                physical_id: "right".into(),
                statistics: OperatorStatistics::PassThrough { edges: unary },
                output_buffer_bytes: 0,
            },
        ),
        (
            "root".into(),
            PhysicalNodeEvidence {
                physical_id: "root".into(),
                statistics: OperatorStatistics::Concat {
                    inputs: vec![edge, edge],
                    output: EdgeStatistics {
                        rows: 20,
                        bytes: 160,
                    },
                    promql: None,
                },
                output_buffer_bytes: 0,
            },
        ),
    ]);
    (
        EvidenceBackedPhysicalDag {
            nodes,
            root: "root".into(),
            evidence,
        },
        scope,
    )
}

use asap_aware_mapping::boundary_cost::*;

fn profile(dag: &EvidenceBackedPhysicalDag) -> BoundaryProfile {
    BoundaryProfile {
        evidence_version: "evidence-v1".into(),
        observed_at_ms: 90,
        valid_until_ms: 200,
        calibration: BoundaryCalibration {
            version: "bytes-v1".into(),
            cost_per_network_byte: 2.0,
            cost_per_materialization_byte: 3.0,
        },
        plans: vec![BoundaryPlanEvidence {
            root: dag.root.clone(),
            nodes: dag
                .nodes
                .iter()
                .map(|node| {
                    (
                        node.id.clone(),
                        BoundaryNodeEvidence {
                            node: node.clone(),
                            statistics: dag.evidence[&node.id].statistics.clone(),
                            boundaries: vec![],
                        },
                    )
                })
                .collect(),
        }],
    }
}

fn transfer(id: &str, consumer: Option<&str>) -> PhysicalBoundary {
    PhysicalBoundary {
        id: id.into(),
        consumer: consumer.map(str::to_owned),
        kind: BoundaryKind::Network {
            source_location: "edge".into(),
            destination_location: "backend".into(),
        },
        logical_bytes: 80,
        encoded_bytes: 40,
        copies: 2,
    }
}

fn profiles_for_alternatives(dags: &[&EvidenceBackedPhysicalDag]) -> BoundaryProfile {
    let mut combined = profile(dags[0]);
    combined.plans.clear();
    for dag in dags {
        let mut alternative = profile(dag);
        alternative.plans[0]
            .nodes
            .get_mut("scan")
            .unwrap()
            .boundaries = vec![transfer(&format!("wire-{}", dag.root), Some(&dag.root))];
        combined.plans.extend(alternative.plans);
    }
    combined
}

// Alternative consumers may share a producer identity without sharing transfers.
#[test]
fn shared_producer_boundaries_are_scoped_to_each_alternative() {
    let (dag, scope) = fixture();
    let alternative = |root: &str| {
        let mut value = dag.clone();
        value.root = root.into();
        value
            .nodes
            .retain(|node| node.id == "scan" || node.id == root);
        value.evidence.retain(|id, _| id == "scan" || id == root);
        value
    };
    let raw = alternative("left");
    let candidate = alternative("right");
    let profile = profiles_for_alternatives(&[&raw, &candidate]);
    for plan in [&raw, &candidate] {
        let estimate = estimate_boundaries(plan, &scope, &profile, "evidence-v1").unwrap();
        assert_eq!(estimate.total.network_bytes, 240);
        assert_eq!(estimate.per_boundary.len(), 1);
        assert!(estimate
            .per_boundary
            .contains_key(&format!("wire-{}", plan.root)));
    }
}

// A boundary binding must identify exactly one complete physical alternative.
#[test]
fn missing_ambiguous_and_mismatched_plan_bindings_are_rejected() {
    let (dag, scope) = fixture();
    for case in 0..6 {
        let mut profile = profile(&dag);
        match case {
            0 => profile.plans.clear(),
            1 => profile.plans.push(profile.plans[0].clone()),
            2 => profile.plans[0].root = "left".into(),
            3 => {
                profile.plans[0].nodes.remove("left");
            }
            4 => {
                profile.plans[0]
                    .nodes
                    .get_mut("scan")
                    .unwrap()
                    .node
                    .execution = ExecutionMultiplicity::Once
            }
            5 => {
                let OperatorStatistics::Scan {
                    source_read_bytes, ..
                } = &mut profile.plans[0].nodes.get_mut("scan").unwrap().statistics
                else {
                    unreachable!()
                };
                *source_read_bytes += 1;
            }
            _ => unreachable!(),
        }
        assert!(
            estimate_boundaries(&dag, &scope, &profile, "evidence-v1").is_err(),
            "case {case}"
        );
    }
}

// Ordinary in-memory edges contribute no traffic; shared transfers count once.
#[test]
fn memory_edges_are_free_and_shared_transfer_is_counted_once() {
    let (dag, scope) = fixture();
    let mut profile = profile(&dag);
    assert_eq!(
        estimate_boundaries(&dag, &scope, &profile, "evidence-v1")
            .unwrap()
            .total,
        BoundaryResources::default()
    );
    profile.plans[0]
        .nodes
        .get_mut("scan")
        .unwrap()
        .boundaries
        .push(transfer("shared", None));
    let estimate = estimate_boundaries(&dag, &scope, &profile, "evidence-v1").unwrap();
    assert_eq!(estimate.total.network_bytes, 240); // 40 encoded bytes × 2 replicas × 3 evaluations
    assert_eq!(estimate.total.materialization_bytes, 0);
    assert_eq!(estimate.cost, 480.0);
    assert_eq!(estimate.per_node["scan"].network_bytes, 240);
    assert_eq!(estimate.per_boundary["shared"].network_bytes, 240);
}

// A retained producer materializes once and transfers separately to each reader.
#[test]
fn materialization_once_and_transfers_per_consumer_have_distinct_multiplicity() {
    let (mut dag, scope) = fixture();
    dag.nodes[0].execution = ExecutionMultiplicity::Once;
    dag.nodes[0].retained_bytes = 80;
    let mut profile = profile(&dag);
    let mut materialize = transfer("persist", None);
    materialize.kind = BoundaryKind::Materialization {
        medium: MaterializationMedium::Disk,
    };
    materialize.copies = 1;
    profile.plans[0].nodes.get_mut("scan").unwrap().boundaries = vec![
        materialize,
        transfer("left-wire", Some("left")),
        transfer("right-wire", Some("right")),
    ];
    let estimate = estimate_boundaries(&dag, &scope, &profile, "evidence-v1").unwrap();
    assert_eq!(estimate.total.materialization_bytes, 40);
    assert_eq!(estimate.total.network_bytes, 480);
    assert_eq!(estimate.cost, 1080.0);
}

// Invalid endpoint, byte evidence, duplicate identity, and overflow fail closed.
#[test]
fn incompatible_boundary_evidence_is_rejected() {
    let (dag, scope) = fixture();
    for case in 0..7 {
        let mut profile = profile(&dag);
        let mut boundary = transfer("wire", Some("left"));
        match case {
            0 => boundary.consumer = Some("root".into()),
            1 => boundary.logical_bytes = 79,
            2 => boundary.copies = 0,
            3 => boundary.encoded_bytes = 0,
            4 => {
                boundary.kind = BoundaryKind::Network {
                    source_location: "same".into(),
                    destination_location: "same".into(),
                }
            }
            5 => boundary.copies = u64::MAX,
            6 => profile.plans[0]
                .nodes
                .get_mut("scan")
                .unwrap()
                .boundaries
                .push(boundary.clone()),
            _ => unreachable!(),
        }
        profile.plans[0]
            .nodes
            .get_mut("scan")
            .unwrap()
            .boundaries
            .push(boundary);
        assert!(
            estimate_boundaries(&dag, &scope, &profile, "evidence-v1").is_err(),
            "case {case}"
        );
    }
}

// Missing evidence and non-finite coefficients cannot be treated as zero.
#[test]
fn missing_stale_and_non_finite_evidence_is_rejected() {
    let (dag, scope) = fixture();
    let mut evidence = profile(&dag);
    evidence.plans[0].nodes.remove("left");
    assert!(estimate_boundaries(&dag, &scope, &evidence, "evidence-v1").is_err());
    evidence = profile(&dag);
    evidence.valid_until_ms = 100;
    assert!(estimate_boundaries(&dag, &scope, &evidence, "evidence-v1").is_err());
    evidence = profile(&dag);
    evidence.calibration.cost_per_network_byte = f64::INFINITY;
    assert!(estimate_boundaries(&dag, &scope, &evidence, "evidence-v1").is_err());
    assert!(estimate_boundaries(&dag, &scope, &profile(&dag), "different").is_err());
}
