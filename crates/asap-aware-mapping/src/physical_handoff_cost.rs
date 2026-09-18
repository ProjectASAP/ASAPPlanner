//! Byte estimates at deployment-declared physical handoffs.

use crate::analytical_cost::{
    estimate_physical_dag, AnalyticalCostError, EvidenceBackedPhysicalDag, ExecutionMultiplicity,
    PhysicalDagNode,
};
use crate::physical_operator_statistics::{ComparisonScope, OperatorStatistics};
pub use asap_types::resources::{MaterializationMedium, PhysicalHandoffBytes, PhysicalHandoffKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const PHYSICAL_HANDOFF_MODEL_VERSION: &str = "physical-boundary-bytes-v1";

/// A physical action on a producer output, distinct from a logical DAG edge.
/// With `consumer = None`, one action serves all consumers. With a consumer,
/// it is a separate action per execution of that downstream physical node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoff {
    pub id: String,
    pub consumer: Option<String>,
    pub kind: PhysicalHandoffKind,
    pub logical_bytes: u64,
    /// Encoded payload per execution; compression is explicit evidence.
    pub encoded_bytes: u64,
    /// Copies actually transferred or written (e.g. broadcast replicas).
    pub copies: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoffNodeEvidence {
    pub node: PhysicalDagNode,
    pub statistics: OperatorStatistics,
    #[serde(rename = "boundaries")]
    pub handoffs: Vec<PhysicalHandoff>,
}

/// Complete supplementary physical binding captured atomically with the
/// planner snapshot. An empty per-node list explicitly asserts no handoff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoffProfile {
    pub evidence_version: String,
    pub observed_at_ms: u64,
    pub valid_until_ms: u64,
    pub calibration: PhysicalHandoffCalibration,
    pub plans: Vec<PhysicalHandoffPlanEvidence>,
}

/// Handoffs belong to a complete alternative: a producer's consumers may
/// differ between plans even when its physical identity is unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoffPlanEvidence {
    pub root: String,
    pub nodes: HashMap<String, PhysicalHandoffNodeEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalHandoffCalibration {
    pub version: String,
    pub cost_per_network_byte: f64,
    pub cost_per_materialization_byte: f64,
}

impl PhysicalHandoffCalibration {
    pub fn cost(&self, value: PhysicalHandoffBytes) -> Result<f64, AnalyticalCostError> {
        if self.version.trim().is_empty()
            || [
                self.cost_per_network_byte,
                self.cost_per_materialization_byte,
            ]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(invalid("invalid handoff calibration"));
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
pub struct PhysicalHandoffEstimate {
    pub total: PhysicalHandoffBytes,
    pub per_node: HashMap<String, PhysicalHandoffBytes>,
    #[serde(rename = "per_boundary")]
    pub per_handoff: HashMap<String, PhysicalHandoffBytes>,
    pub model_version: String,
    pub evidence_version: String,
    pub calibration_version: String,
    pub cost: f64,
}

fn invalid(reason: &'static str) -> AnalyticalCostError {
    AnalyticalCostError::InvalidPhysicalDag(reason)
}

pub fn estimate_physical_handoffs(
    dag: &EvidenceBackedPhysicalDag,
    scope: &ComparisonScope,
    profile: &PhysicalHandoffProfile,
    evidence_version: &str,
) -> Result<PhysicalHandoffEstimate, AnalyticalCostError> {
    estimate_physical_dag(&dag.nodes, &dag.root, scope, dag)?;
    let evaluations = scope.validate()?;
    if profile.evidence_version.trim().is_empty()
        || profile.evidence_version != evidence_version
        || profile.observed_at_ms > scope.planning_time.0
        || profile.valid_until_ms <= scope.planning_time.0
    {
        return Err(AnalyticalCostError::MissingOrStale("handoff profile"));
    }
    let mut matching_plans = profile.plans.iter().filter(|plan| {
        plan.root == dag.root
            && plan.nodes.len() == dag.nodes.len()
            && dag.nodes.iter().all(|node| {
                plan.nodes.get(&node.id).is_some_and(|evidence| {
                    evidence.node == *node
                        && dag.evidence.get(&node.id).is_some_and(|physical| {
                            physical.physical_id == node.id
                                && physical.output_buffer_bytes == node.output_buffer_bytes
                                && evidence.statistics == physical.statistics
                        })
                })
            })
    });
    let plan = matching_plans
        .next()
        .ok_or(AnalyticalCostError::MissingOrStale("handoff physical plan"))?;
    if matching_plans.next().is_some() {
        return Err(invalid("ambiguous handoff physical plan"));
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
    let mut total = PhysicalHandoffBytes::default();
    let mut per_node = HashMap::new();
    let mut per_handoff = HashMap::new();
    for id in &reachable {
        let node = by_id[id];
        let evidence = &plan.nodes[*id];
        let mut local = PhysicalHandoffBytes::default();
        for handoff in &evidence.handoffs {
            if handoff.id.trim().is_empty() || per_handoff.contains_key(&handoff.id) {
                return Err(invalid("duplicate or blank physical handoff identity"));
            }
            if handoff.logical_bytes != evidence.statistics.output().bytes
                || (handoff.logical_bytes == 0) != (handoff.encoded_bytes == 0)
                || handoff.copies == 0
            {
                return Err(invalid(
                    "handoff payload is incompatible with producer output",
                ));
            }
            let execution = if let Some(consumer) = &handoff.consumer {
                if !reachable.contains(consumer.as_str())
                    || !by_id[consumer.as_str()]
                        .children
                        .iter()
                        .any(|child| child == *id)
                {
                    return Err(invalid("handoff consumer is not a reachable physical edge"));
                }
                by_id[consumer.as_str()].execution
            } else {
                node.execution
            };
            let executions = match execution {
                ExecutionMultiplicity::Once => 1,
                ExecutionMultiplicity::PerEvaluation => evaluations,
            };
            let bytes = handoff
                .encoded_bytes
                .checked_mul(handoff.copies)
                .and_then(|bytes| bytes.checked_mul(executions))
                .ok_or(AnalyticalCostError::Overflow)?;
            let mut term = PhysicalHandoffBytes::default();
            match &handoff.kind {
                PhysicalHandoffKind::Network {
                    source_location,
                    destination_location,
                } => {
                    if source_location.trim().is_empty()
                        || destination_location.trim().is_empty()
                        || source_location.trim() == destination_location.trim()
                    {
                        return Err(invalid(
                            "network handoff needs distinct non-empty locations",
                        ));
                    }
                    term.network_bytes = bytes;
                }
                PhysicalHandoffKind::Materialization { .. } => term.materialization_bytes = bytes,
            }
            local = local
                .checked_add(term)
                .ok_or(AnalyticalCostError::Overflow)?;
            per_handoff.insert(handoff.id.clone(), term);
        }
        total = total
            .checked_add(local)
            .ok_or(AnalyticalCostError::Overflow)?;
        per_node.insert((*id).into(), local);
    }
    Ok(PhysicalHandoffEstimate {
        total,
        per_node,
        per_handoff,
        model_version: PHYSICAL_HANDOFF_MODEL_VERSION.into(),
        evidence_version: profile.evidence_version.clone(),
        calibration_version: profile.calibration.version.clone(),
        cost: profile.calibration.cost(total)?,
    })
}
