//! Analytical resource cost for incrementally maintained summary deployments.
//!
//! The canonical workload and lifecycle types own deployment semantics. This
//! module only adds physical evidence absent from those schemas: state size,
//! window counts, and per-operation CPU measurements or complexity estimates.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::post_asap::{
    SketchAlgorithm, SummaryExpr, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummaryNode,
};
use asap_types::pre_asap::{
    agg_intent::AggIntent, CompareOpKind, InfoMatcher, Predicate, QueryExpr, Source,
};
use asap_types::workload::{DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand};
use serde::{Deserialize, Serialize};

use crate::analytical_cost::ExecutionMultiplicity;
use crate::analytical_cost::{
    estimate_physical_dag, AnalyticalCostError, EvidenceBackedPhysicalDag, PhysicalOperator,
    ResourceCalibration, ResourceEstimate,
};
#[cfg(test)]
use crate::analytical_cost::{PhysicalDagNode, PhysicalNodeEvidence};
use crate::cost_model::CostedSummaryDeployment;
use crate::cost_model::{Cost, CostModel, DefaultCostModel};
#[cfg(test)]
use crate::physical_operator_statistics::UnaryEdgeStatistics;
use crate::physical_operator_statistics::{ComparisonScope, EdgeStatistics, OperatorStatistics};
use crate::recurrence::CostRate;
use crate::replacement::{Replacement, ReplacementSubDAG, TargetSubDAG};
use crate::summary_maintenance_lifecycle::{
    evaluation_schedule, maintenance_mode, SummaryMaintenanceCapabilities,
    SummaryMaintenanceLifecycleCostInputs,
};

pub const SUMMARY_MAINTENANCE_COST_MODEL_VERSION: &str = "summary-maintenance-resource-v1";

mod evidence;
mod model;

pub use evidence::*;
pub use model::*;
