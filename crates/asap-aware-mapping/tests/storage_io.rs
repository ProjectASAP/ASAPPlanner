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

use asap_aware_mapping::storage_io::*;

// The compatibility import and shared resource namespace expose one Rust type.
#[test]
fn storage_estimates_use_the_shared_resource_type_without_wire_changes() {
    let (dag, scope) = fixture();
    let estimate = estimate_storage_io(&dag, &scope, &profile(&dag), "evidence-v1").unwrap();
    let shared: asap_types::resources::StorageResources = estimate.total;
    let legacy: asap_aware_mapping::storage_io::StorageResources = shared;
    assert_eq!(shared, legacy);
    let wire = serde_json::to_value(&estimate).unwrap();
    assert_eq!(
        wire["total"],
        serde_json::json!({
            "disk_reads": 0, "disk_writes": 0, "object_gets": 12, "object_puts": 0,
        })
    );
    assert_eq!(
        serde_json::from_value::<StorageEstimate>(wire).unwrap(),
        estimate
    );
}

fn profile(dag: &EvidenceBackedPhysicalDag) -> StorageIoProfile {
    StorageIoProfile {
        evidence_version: "evidence-v1".into(),
        observed_at_ms: 90,
        valid_until_ms: 200,
        calibration: StorageCalibration {
            version: "requests-v1".into(),
            cost_per_disk_read: 1.0,
            cost_per_disk_write: 2.0,
            cost_per_object_get: 3.0,
            cost_per_object_put: 4.0,
        },
        nodes: dag
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.clone(),
                    StorageNodeEvidence {
                        node: node.clone(),
                        statistics: dag.evidence[&node.id].statistics.clone(),
                        accesses: if node.id == "scan" {
                            vec![StorageAccess {
                                operation: StorageOperation::ObjectGet,
                                extent_bytes: vec![40, 40],
                                bytes_per_request: 32,
                            }]
                        } else {
                            vec![]
                        },
                    },
                )
            })
            .collect(),
    }
}

// The shared scan reads two objects once per evaluation, despite two parents.
#[test]
fn shared_io_counts_rounding_then_evaluations_and_explicit_writes() {
    let (dag, scope) = fixture();
    let mut profile = profile(&dag);
    profile
        .nodes
        .get_mut("root")
        .unwrap()
        .accesses
        .push(StorageAccess {
            operation: StorageOperation::DiskWrite,
            extent_bytes: vec![160],
            bytes_per_request: 64,
        });
    let estimate = estimate_storage_io(&dag, &scope, &profile, "evidence-v1").unwrap();
    assert_eq!(estimate.total.object_gets, 12);
    assert_eq!(estimate.total.disk_writes, 9);
    assert_eq!(estimate.cost, 54.0);
    assert_eq!(estimate.per_node["scan"].object_gets, 12);
    assert_eq!(estimate.per_node["left"], StorageResources::default());
    assert_eq!(estimate.model_version, STORAGE_IO_MODEL_VERSION);
}

// A retained scan is read once; downstream in-memory consumers add no reads.
#[test]
fn retained_state_does_not_reread_storage() {
    let (mut dag, scope) = fixture();
    dag.nodes[0].execution = ExecutionMultiplicity::Once;
    dag.nodes[0].retained_bytes = 80;
    let estimate = estimate_storage_io(&dag, &scope, &profile(&dag), "evidence-v1").unwrap();
    assert_eq!(estimate.total.object_gets, 4);
}

// Incomplete, expired, rebound, or inconsistent scan evidence cannot be ranked.
#[test]
fn incomplete_stale_and_incompatible_evidence_is_rejected() {
    let (dag, scope) = fixture();
    let original = profile(&dag);
    let mut missing = original.clone();
    missing.nodes.remove("left");
    assert!(estimate_storage_io(&dag, &scope, &missing, "evidence-v1").is_err());
    let mut stale = original.clone();
    stale.valid_until_ms = 100;
    assert!(estimate_storage_io(&dag, &scope, &stale, "evidence-v1").is_err());
    stale.valid_until_ms = 200;
    stale.observed_at_ms = 101;
    assert!(estimate_storage_io(&dag, &scope, &stale, "evidence-v1").is_err());
    let mut mismatch = original.clone();
    mismatch.nodes.get_mut("scan").unwrap().accesses[0].extent_bytes = vec![79];
    assert!(estimate_storage_io(&dag, &scope, &mismatch, "evidence-v1").is_err());
    mismatch = original.clone();
    mismatch.nodes.get_mut("scan").unwrap().node.execution = ExecutionMultiplicity::Once;
    assert!(estimate_storage_io(&dag, &scope, &mismatch, "evidence-v1").is_err());
    assert!(estimate_storage_io(&dag, &scope, &original, "another-version").is_err());
}

// Zero and non-finite values in wire evidence are rejected without defaults.
#[test]
fn wire_evidence_is_strict() {
    let (dag, _) = fixture();
    let mut json = serde_json::to_value(profile(&dag)).unwrap();
    json["nodes"]["scan"]["accesses"][0]["bytes_per_request"] = serde_json::json!(1.5);
    assert!(serde_json::from_value::<StorageIoProfile>(json).is_err());
    let mut json = serde_json::to_value(profile(&dag)).unwrap();
    json["calibration"]["cost_per_disk_read"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<StorageIoProfile>(json).is_err());
}

// Zero-length payloads add no requests, without turning absent reads into zero.
#[test]
fn zero_extents_and_all_request_kinds_preserve_dimensions() {
    let (dag, scope) = fixture();
    let mut profile = profile(&dag);
    profile.nodes.get_mut("scan").unwrap().accesses[0]
        .extent_bytes
        .push(0);
    profile.nodes.get_mut("root").unwrap().accesses = [
        StorageOperation::DiskRead,
        StorageOperation::DiskWrite,
        StorageOperation::ObjectGet,
        StorageOperation::ObjectPut,
    ]
    .into_iter()
    .map(|operation| StorageAccess {
        operation,
        extent_bytes: vec![0, 1, 9],
        bytes_per_request: 8,
    })
    .collect();
    let estimate = estimate_storage_io(&dag, &scope, &profile, "evidence-v1").unwrap();
    assert_eq!(
        estimate.total,
        StorageResources {
            disk_reads: 9,
            disk_writes: 9,
            object_gets: 21,
            object_puts: 9,
        }
    );
    assert_eq!(estimate.cost, 126.0);
    profile.nodes.get_mut("scan").unwrap().accesses.clear();
    assert!(estimate_storage_io(&dag, &scope, &profile, "evidence-v1").is_err());
}

// Valid local integer counts must not wrap when scaled or composed across nodes.
#[test]
fn multiplicity_and_cross_node_overflow_are_unavailable() {
    let (dag, mut scope) = fixture();
    let mut profile = profile(&dag);
    let large = StorageAccess {
        operation: StorageOperation::DiskWrite,
        extent_bytes: vec![u64::MAX],
        bytes_per_request: 1,
    };
    profile
        .nodes
        .get_mut("root")
        .unwrap()
        .accesses
        .push(large.clone());
    assert_eq!(
        estimate_storage_io(&dag, &scope, &profile, "evidence-v1"),
        Err(asap_aware_mapping::analytical_cost::AnalyticalCostError::Overflow)
    );
    scope.recurrence = QueryRecurrence::OneTime {
        invocations: 1,
        execute_at: None,
    };
    profile
        .nodes
        .get_mut("left")
        .unwrap()
        .accesses
        .push(StorageAccess {
            extent_bytes: vec![1],
            ..large
        });
    assert_eq!(
        estimate_storage_io(&dag, &scope, &profile, "evidence-v1"),
        Err(asap_aware_mapping::analytical_cost::AnalyticalCostError::Overflow)
    );
}

// Reusing map keys must not bind another node, source snapshot, or statistics.
#[test]
fn storage_node_identity_statistics_and_calibration_provenance_are_bound() {
    let (dag, scope) = fixture();
    let original = profile(&dag);
    let mut bad = original.clone();
    bad.nodes.get_mut("scan").unwrap().node.id = "another-scan".into();
    assert!(estimate_storage_io(&dag, &scope, &bad, "evidence-v1").is_err());
    bad = original.clone();
    bad.nodes
        .get_mut("scan")
        .unwrap()
        .node
        .source_coverage
        .as_mut()
        .unwrap()
        .source_snapshot_id = "another-source".into();
    assert!(estimate_storage_io(&dag, &scope, &bad, "evidence-v1").is_err());
    bad = original.clone();
    if let OperatorStatistics::Scan {
        source_read_bytes, ..
    } = &mut bad.nodes.get_mut("scan").unwrap().statistics
    {
        *source_read_bytes = 79;
    }
    assert!(estimate_storage_io(&dag, &scope, &bad, "evidence-v1").is_err());
    bad = original.clone();
    bad.calibration.version = " \t".into();
    assert!(estimate_storage_io(&dag, &scope, &bad, "evidence-v1").is_err());
    let mut missing = dag.clone();
    missing.evidence.remove("scan");
    assert!(estimate_storage_io(&missing, &scope, &original, "evidence-v1").is_err());
}
