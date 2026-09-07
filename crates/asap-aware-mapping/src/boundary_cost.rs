//! Byte estimates at deployment-declared physical boundaries.

use crate::analytical_cost::{
    estimate_physical_dag, AnalyticalCostError, EvidenceBackedPhysicalDag, ExecutionMultiplicity,
    PhysicalDagNode,
};
use crate::physical_operator_statistics::{ComparisonScope, OperatorStatistics};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const BOUNDARY_MODEL_VERSION: &str = "physical-boundary-bytes-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundaryKind {
    Network {
        source_location: String,
        destination_location: String,
    },
    Materialization {
        medium: MaterializationMedium,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationMedium {
    Memory,
    Disk,
    ObjectStore,
}

/// A physical action on a producer output, distinct from a logical DAG edge.
/// With `consumer = None`, one action serves all consumers. With a consumer,
/// it is a separate action per execution of that downstream physical node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalBoundary {
    pub id: String,
    pub consumer: Option<String>,
    pub kind: BoundaryKind,
    pub logical_bytes: u64,
    /// Encoded payload per execution; compression is explicit evidence.
    pub encoded_bytes: u64,
    /// Copies actually transferred or written (e.g. broadcast replicas).
    pub copies: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryNodeEvidence {
    pub node: PhysicalDagNode,
    pub statistics: OperatorStatistics,
    pub boundaries: Vec<PhysicalBoundary>,
}

/// Complete supplementary physical binding captured atomically with the
/// planner snapshot. An empty per-node list explicitly asserts no boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryProfile {
    pub evidence_version: String,
    pub observed_at_ms: u64,
    pub valid_until_ms: u64,
    pub calibration: BoundaryCalibration,
    pub nodes: HashMap<String, BoundaryNodeEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryCalibration {
    pub version: String,
    pub cost_per_network_byte: f64,
    pub cost_per_materialization_byte: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryResources {
    pub network_bytes: u64,
    pub materialization_bytes: u64,
}

impl BoundaryResources {
    pub fn terms(self) -> [(&'static str, u64); 2] {
        [
            ("network_bytes", self.network_bytes),
            ("materialization_bytes", self.materialization_bytes),
        ]
    }
    fn add(&mut self, other: Self) -> Result<(), AnalyticalCostError> {
        self.network_bytes = self
            .network_bytes
            .checked_add(other.network_bytes)
            .ok_or(AnalyticalCostError::Overflow)?;
        self.materialization_bytes = self
            .materialization_bytes
            .checked_add(other.materialization_bytes)
            .ok_or(AnalyticalCostError::Overflow)?;
        Ok(())
    }
}

impl BoundaryCalibration {
    pub fn cost(&self, value: BoundaryResources) -> Result<f64, AnalyticalCostError> {
        if self.version.trim().is_empty()
            || [
                self.cost_per_network_byte,
                self.cost_per_materialization_byte,
            ]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(invalid("invalid boundary calibration"));
        }
        let cost = value.network_bytes as f64 * self.cost_per_network_byte
            + value.materialization_bytes as f64 * self.cost_per_materialization_byte;
        if !cost.is_finite() {
            return Err(AnalyticalCostError::Overflow);
        }
        Ok(cost)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryEstimate {
    pub total: BoundaryResources,
    pub per_node: HashMap<String, BoundaryResources>,
    pub per_boundary: HashMap<String, BoundaryResources>,
    pub model_version: String,
    pub evidence_version: String,
    pub calibration_version: String,
    pub cost: f64,
}

fn invalid(reason: &'static str) -> AnalyticalCostError {
    AnalyticalCostError::InvalidPhysicalDag(reason)
}

pub fn estimate_boundaries(
    dag: &EvidenceBackedPhysicalDag,
    scope: &ComparisonScope,
    profile: &BoundaryProfile,
    evidence_version: &str,
) -> Result<BoundaryEstimate, AnalyticalCostError> {
    estimate_physical_dag(&dag.nodes, &dag.root, scope, dag)?;
    let evaluations = scope.validate()?;
    if profile.evidence_version.trim().is_empty()
        || profile.evidence_version != evidence_version
        || profile.observed_at_ms > scope.planning_time.0
        || profile.valid_until_ms <= scope.planning_time.0
    {
        return Err(AnalyticalCostError::MissingOrStale("boundary profile"));
    }
    let by_id: HashMap<_, _> = dag
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut reachable = HashSet::new();
    let mut pending = vec![dag.root.as_str()];
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(by_id[id].children.iter().map(String::as_str));
        }
    }
    let mut total = BoundaryResources::default();
    let mut per_node = HashMap::new();
    let mut per_boundary = HashMap::new();
    for id in &reachable {
        let node = by_id[id];
        let evidence = profile.nodes.get(*id).ok_or_else(|| {
            AnalyticalCostError::MissingOperatorStatistics(format!("boundary:{id}"))
        })?;
        if evidence.node != *node || evidence.statistics != dag.evidence[*id].statistics {
            return Err(invalid(
                "boundary evidence differs from physical node snapshot",
            ));
        }
        let mut local = BoundaryResources::default();
        for boundary in &evidence.boundaries {
            if boundary.id.trim().is_empty() || per_boundary.contains_key(&boundary.id) {
                return Err(invalid("duplicate or blank physical boundary identity"));
            }
            if boundary.logical_bytes != evidence.statistics.output().bytes
                || (boundary.logical_bytes == 0) != (boundary.encoded_bytes == 0)
                || boundary.copies == 0
            {
                return Err(invalid(
                    "boundary payload is incompatible with producer output",
                ));
            }
            let execution = if let Some(consumer) = &boundary.consumer {
                if !reachable.contains(consumer.as_str())
                    || !by_id[consumer.as_str()]
                        .children
                        .iter()
                        .any(|child| child == *id)
                {
                    return Err(invalid(
                        "boundary consumer is not a reachable physical edge",
                    ));
                }
                by_id[consumer.as_str()].execution
            } else {
                node.execution
            };
            let executions = match execution {
                ExecutionMultiplicity::Once => 1,
                ExecutionMultiplicity::PerEvaluation => evaluations,
            };
            let bytes = boundary
                .encoded_bytes
                .checked_mul(boundary.copies)
                .and_then(|bytes| bytes.checked_mul(executions))
                .ok_or(AnalyticalCostError::Overflow)?;
            let mut term = BoundaryResources::default();
            match &boundary.kind {
                BoundaryKind::Network {
                    source_location,
                    destination_location,
                } => {
                    if source_location.trim().is_empty()
                        || destination_location.trim().is_empty()
                        || source_location.trim() == destination_location.trim()
                    {
                        return Err(invalid(
                            "network boundary needs distinct non-empty locations",
                        ));
                    }
                    term.network_bytes = bytes;
                }
                BoundaryKind::Materialization { .. } => term.materialization_bytes = bytes,
            }
            local.add(term)?;
            per_boundary.insert(boundary.id.clone(), term);
        }
        total.add(local)?;
        per_node.insert((*id).into(), local);
    }
    Ok(BoundaryEstimate {
        total,
        per_node,
        per_boundary,
        model_version: BOUNDARY_MODEL_VERSION.into(),
        evidence_version: profile.evidence_version.clone(),
        calibration_version: profile.calibration.version.clone(),
        cost: profile.calibration.cost(total)?,
    })
}
