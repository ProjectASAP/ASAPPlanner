//! Request counts for explicitly bound physical storage accesses.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

// Keep the original public import path while sharing the sole type definition.
pub use asap_types::resources::StorageResources;

use crate::analytical_cost::{
    estimate_physical_dag, AnalyticalCostError, EvidenceBackedPhysicalDag, ExecutionMultiplicity,
    PhysicalDagNode, PhysicalOperator,
};
use crate::physical_operator_statistics::{ComparisonScope, OperatorStatistics};

pub const STORAGE_IO_MODEL_VERSION: &str = "storage-requests-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageOperation {
    DiskRead,
    DiskWrite,
    ObjectGet,
    ObjectPut,
}

/// Each extent is one independent contiguous disk range, object, or multipart
/// payload. Requests cannot coalesce across extent boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageAccess {
    pub operation: StorageOperation,
    pub extent_bytes: Vec<u64>,
    pub bytes_per_request: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageCalibration {
    pub version: String,
    pub cost_per_disk_read: f64,
    pub cost_per_disk_write: f64,
    pub cost_per_object_get: f64,
    pub cost_per_object_put: f64,
}

impl StorageCalibration {
    pub fn cost(&self, value: StorageResources) -> Result<f64, AnalyticalCostError> {
        if self.version.trim().is_empty() {
            return Err(invalid("blank storage calibration version"));
        }
        let mut cost = 0.0;
        for (coefficient, count) in [
            (self.cost_per_disk_read, value.disk_reads),
            (self.cost_per_disk_write, value.disk_writes),
            (self.cost_per_object_get, value.object_gets),
            (self.cost_per_object_put, value.object_puts),
        ] {
            if !coefficient.is_finite() || coefficient < 0.0 {
                return Err(invalid("invalid storage calibration coefficient"));
            }
            cost += coefficient * count as f64;
        }
        if !cost.is_finite() {
            return Err(AnalyticalCostError::Overflow);
        }
        Ok(cost)
    }
}

/// Atomic deployment snapshot. Every reachable physical node needs an entry,
/// including an explicit empty access list for memory-only operators.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageIoProfile {
    pub evidence_version: String,
    pub observed_at_ms: u64,
    pub valid_until_ms: u64,
    pub calibration: StorageCalibration,
    pub nodes: HashMap<String, StorageNodeEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageNodeEvidence {
    pub node: PhysicalDagNode,
    pub statistics: OperatorStatistics,
    pub accesses: Vec<StorageAccess>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageEstimate {
    pub total: StorageResources,
    pub per_node: HashMap<String, StorageResources>,
    pub model_version: String,
    pub evidence_version: String,
    pub calibration_version: String,
    pub cost: f64,
}

fn invalid(reason: &'static str) -> AnalyticalCostError {
    AnalyticalCostError::InvalidPhysicalDag(reason)
}

/// ceil(bytes/request_size), rounded independently per extent and execution.
/// No metadata, retry, or speculative prefetch operations are inferred.
pub fn request_count(access: &StorageAccess) -> Result<u64, AnalyticalCostError> {
    if access.bytes_per_request == 0 {
        return Err(invalid("storage request size must be positive"));
    }
    access.extent_bytes.iter().try_fold(0_u64, |sum, bytes| {
        let count =
            bytes / access.bytes_per_request + u64::from(bytes % access.bytes_per_request != 0);
        sum.checked_add(count).ok_or(AnalyticalCostError::Overflow)
    })
}

pub fn estimate_storage_io(
    dag: &EvidenceBackedPhysicalDag,
    scope: &ComparisonScope,
    profile: &StorageIoProfile,
    evidence_version: &str,
) -> Result<StorageEstimate, AnalyticalCostError> {
    // Also prove source coverage, edge consistency, execution legality and DAG
    // identity before using supplementary deployment evidence.
    estimate_physical_dag(&dag.nodes, &dag.root, scope, dag)?;
    let evaluations = scope.validate()?;
    if profile.evidence_version.trim().is_empty()
        || profile.evidence_version != evidence_version
        || profile.observed_at_ms > scope.planning_time.0
        || profile.valid_until_ms <= scope.planning_time.0
    {
        return Err(AnalyticalCostError::MissingOrStale("storage I/O profile"));
    }
    let by_id: HashMap<_, _> = dag
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut seen = HashSet::new();
    let mut pending = vec![dag.root.as_str()];
    let mut total = StorageResources::default();
    let mut per_node = HashMap::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let node = by_id[id];
        pending.extend(node.children.iter().map(String::as_str));
        let evidence = profile.nodes.get(id).ok_or_else(|| {
            AnalyticalCostError::MissingOperatorStatistics(format!("storage:{id}"))
        })?;
        if evidence.node != *node || evidence.statistics != dag.evidence[id].statistics {
            return Err(invalid(
                "storage evidence differs from physical node snapshot",
            ));
        }
        let executions = match node.execution {
            ExecutionMultiplicity::Once => 1,
            ExecutionMultiplicity::PerEvaluation => evaluations,
        };
        let mut local = StorageResources::default();
        let mut read_bytes = 0_u64;
        for access in &evidence.accesses {
            let count = request_count(access)?
                .checked_mul(executions)
                .ok_or(AnalyticalCostError::Overflow)?;
            let mut term = StorageResources::default();
            match access.operation {
                StorageOperation::DiskRead => term.disk_reads = count,
                StorageOperation::DiskWrite => term.disk_writes = count,
                StorageOperation::ObjectGet => term.object_gets = count,
                StorageOperation::ObjectPut => term.object_puts = count,
            }
            if matches!(
                access.operation,
                StorageOperation::DiskRead | StorageOperation::ObjectGet
            ) {
                for bytes in &access.extent_bytes {
                    read_bytes = read_bytes
                        .checked_add(*bytes)
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
            }
            local = local
                .checked_add(term)
                .ok_or(AnalyticalCostError::Overflow)?;
        }
        if node.operator == PhysicalOperator::Scan {
            let OperatorStatistics::Scan {
                source_read_bytes, ..
            } = &evidence.statistics
            else {
                unreachable!()
            };
            if read_bytes != *source_read_bytes {
                return Err(invalid(
                    "storage scan access bytes differ from source read bytes",
                ));
            }
        }
        total = total
            .checked_add(local)
            .ok_or(AnalyticalCostError::Overflow)?;
        per_node.insert(id.into(), local);
    }
    Ok(StorageEstimate {
        total,
        per_node,
        model_version: STORAGE_IO_MODEL_VERSION.into(),
        evidence_version: profile.evidence_version.clone(),
        calibration_version: profile.calibration.version.clone(),
        cost: profile.calibration.cost(total)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Separate objects need separate requests even when their total fits.
    #[test]
    fn counts_extents_independently_and_is_monotone() {
        let mut access = StorageAccess {
            operation: StorageOperation::ObjectGet,
            extent_bytes: vec![5, 5],
            bytes_per_request: 8,
        };
        assert_eq!(request_count(&access).unwrap(), 2);
        access.bytes_per_request = 4;
        assert_eq!(request_count(&access).unwrap(), 4);
        access.extent_bytes.push(9);
        assert_eq!(request_count(&access).unwrap(), 7);
        access.bytes_per_request = 0;
        assert!(request_count(&access).is_err());
    }
    // Full-width integer evidence must neither wrap nor lose precision.
    #[test]
    fn checked_rounding_handles_maximum_bytes_and_overflow() {
        let mut access = StorageAccess {
            operation: StorageOperation::DiskRead,
            extent_bytes: vec![u64::MAX],
            bytes_per_request: 2,
        };
        assert_eq!(request_count(&access).unwrap(), 1_u64 << 63);
        access.bytes_per_request = 1;
        access.extent_bytes.push(1);
        assert_eq!(request_count(&access), Err(AnalyticalCostError::Overflow));
    }
    // Invalid deployment coefficients cannot produce a usable cost.
    #[test]
    fn rejects_non_finite_calibration() {
        let calibration = StorageCalibration {
            version: "v1".into(),
            cost_per_disk_read: f64::NAN,
            cost_per_disk_write: 0.0,
            cost_per_object_get: 0.0,
            cost_per_object_put: 0.0,
        };
        assert!(calibration.cost(StorageResources::default()).is_err());
    }
}
