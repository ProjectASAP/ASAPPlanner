//! Authoritative comparison-scope and physical-statistics contracts.
//!
//! This module does not lower logical query nodes. It defines the evidence a
//! lowering or catalog provider must supply before two physical DAGs can be
//! compared by the analytical resource model.

use std::collections::HashMap;

use asap_types::pre_asap::query_expr::{Predicate, Source};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
    TimeSelection, TimestampMs,
};
use serde::{Deserialize, Serialize};

use crate::analytical_cost::AnalyticalCostError;

/// The semantic and workload boundary within which two resource estimates
/// may be compared. Canonical workload and query-IR types remain authoritative;
/// only the storage snapshot identifier is new because neither IR names a
/// concrete catalog/storage version.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonScope {
    pub data_arrival: DataArrival,
    pub planning_time: TimestampMs,
    pub horizon: DurationMs,
    pub recurrence: QueryRecurrence,
    pub time_selection: TimeSelection,
    pub sources: Vec<SourceCoverage>,
}

/// Exact source selection covered by a physical plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCoverage {
    pub source: Source,
    /// Catalog version, object generation, snapshot timestamp, or another
    /// provider-owned stable identifier for the physical source contents.
    pub snapshot_id: String,
    /// Canonical predicates copied from the bound/canonicalized query IR.
    pub predicates: Vec<Predicate>,
}

impl ComparisonScope {
    /// Build a comparison boundary from canonical workload fields plus the
    /// physical snapshot identities supplied by the storage/catalog layer.
    pub fn from_workload(
        data: &DataWorkload,
        query: &QueryWorkloadEntry,
        planning_time: TimestampMs,
        horizon: DurationMs,
        sources: Vec<SourceCoverage>,
    ) -> Result<Self, AnalyticalCostError> {
        let scope = Self {
            data_arrival: data.arrival,
            planning_time,
            horizon,
            recurrence: query.recurrence.clone(),
            time_selection: query.time_selection.clone(),
            sources,
        };
        scope.validate()?;
        Ok(scope)
    }

    /// Validate this scope and return its effective query evaluation count.
    pub fn validate(&self) -> Result<u64, AnalyticalCostError> {
        if self.data_arrival != DataArrival::AtRest {
            return Err(AnalyticalCostError::UnsupportedDataArrival(
                self.data_arrival,
            ));
        }
        if self.horizon.0 == 0 {
            return Err(AnalyticalCostError::MissingOrZero("horizon"));
        }
        if self.sources.is_empty() {
            return Err(AnalyticalCostError::MissingComparisonScope("sources"));
        }
        if self
            .sources
            .iter()
            .any(|source| source.snapshot_id.is_empty())
        {
            return Err(AnalyticalCostError::MissingComparisonScope("snapshot_id"));
        }
        evaluations_in_horizon(&self.recurrence, self.planning_time.0, self.horizon.0)
    }
}

/// Require exact scope equality before comparing raw and post-ASAP costs.
/// Exact matching is intentionally conservative: coverage/subsumption needs
/// a separate semantic proof and is not inferred by the resource estimator.
pub fn validate_comparison_scopes(
    raw: &ComparisonScope,
    candidate: &ComparisonScope,
) -> Result<u64, AnalyticalCostError> {
    let evaluations = raw.validate()?;
    candidate.validate()?;
    for (name, matches) in [
        ("data_arrival", raw.data_arrival == candidate.data_arrival),
        (
            "planning_time",
            raw.planning_time == candidate.planning_time,
        ),
        ("horizon", raw.horizon == candidate.horizon),
        ("recurrence", raw.recurrence == candidate.recurrence),
        (
            "time_selection",
            raw.time_selection == candidate.time_selection,
        ),
        ("sources", raw.sources == candidate.sources),
    ] {
        if !matches {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(name));
        }
    }
    Ok(evaluations)
}

pub(crate) fn evaluations_in_horizon(
    recurrence: &QueryRecurrence,
    planning_time_ms: u64,
    horizon_ms: u64,
) -> Result<u64, AnalyticalCostError> {
    if horizon_ms == 0 {
        return Err(AnalyticalCostError::MissingOrZero("horizon_ms"));
    }
    let end = planning_time_ms.saturating_add(horizon_ms);
    let count = match recurrence {
        QueryRecurrence::OneTime {
            invocations,
            execute_at,
        } => {
            if execute_at.is_none_or(|at| at.0 >= planning_time_ms && at.0 <= end) {
                *invocations
            } else {
                0
            }
        }
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
            if interval.0 == 0 {
                return Err(AnalyticalCostError::InvalidRecurrence);
            }
            horizon_ms / u64::from(interval.0)
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => schedule
            .iter()
            .filter(|at| at.0 >= planning_time_ms && at.0 <= end)
            .count()
            as u64,
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
            if !estimate.is_fresh_at(planning_time_ms)
                || !estimate.expected_rate.0.is_finite()
                || estimate.expected_rate.0 < 0.0
            {
                return Err(AnalyticalCostError::InvalidRecurrence);
            }
            let expected = estimate.expected_rate.0 * horizon_ms as f64 / 1000.0;
            if expected > u64::MAX as f64 {
                return Err(AnalyticalCostError::Overflow);
            }
            expected.ceil() as u64
        }
        QueryRecurrence::Unknown => return Err(AnalyticalCostError::InvalidRecurrence),
    };
    if count == 0 {
        return Err(AnalyticalCostError::NoEvaluationsInHorizon);
    }
    Ok(count)
}

/// Logical cardinality and byte width carried by one physical edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeStatistics {
    pub rows: u64,
    pub bytes: u64,
}

impl EdgeStatistics {
    /// Empty logical edges carry neither rows nor bytes. Non-empty edges need
    /// bytes so row-width-dependent formulas do not invent a width.
    pub(crate) fn is_consistent(self) -> bool {
        matches!((self.rows, self.bytes), (0, 0) | (1.., 1..))
    }
}

/// Input and output evidence for a unary physical operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnaryEdgeStatistics {
    pub input: EdgeStatistics,
    pub output: EdgeStatistics,
}

/// Input and output evidence for a binary physical operator. The input order
/// is the physical operator's left/right order and must match its DAG children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryEdgeStatistics {
    pub inputs: [EdgeStatistics; 2],
    pub output: EdgeStatistics,
}

/// Authoritative evidence for one physical operator, structured by operator
/// kind so unrelated facts cannot be combined in one flat bag of `Option`s.
/// Physical configuration such as a Top-K limit or hash-join build side lives
/// on `PhysicalOperator`; this enum contains workload/catalog statistics only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operator", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorStatistics {
    Scan {
        edges: UnaryEdgeStatistics,
        /// Physical bytes read from storage, independent of decoded logical
        /// bytes on the source edge.
        source_read_bytes: u64,
    },
    Filter {
        edges: UnaryEdgeStatistics,
    },
    Project {
        edges: UnaryEdgeStatistics,
    },
    HashAggregate {
        edges: UnaryEdgeStatistics,
        group_count: u64,
        key_bytes: u64,
        accumulator_bytes_per_group: u64,
    },
    InMemoryComparisonSort {
        edges: UnaryEdgeStatistics,
    },
    TopK {
        edges: UnaryEdgeStatistics,
    },
    HashJoin {
        edges: BinaryEdgeStatistics,
    },
    HashDeduplicate {
        edges: UnaryEdgeStatistics,
        distinct_key_count: u64,
        key_bytes: u64,
    },
    Concat {
        inputs: Vec<EdgeStatistics>,
        output: EdgeStatistics,
    },
    InMemoryOrderedWindow {
        edges: UnaryEdgeStatistics,
    },
    Limit {
        edges: UnaryEdgeStatistics,
    },
    PassThrough {
        edges: UnaryEdgeStatistics,
    },
}

impl OperatorStatistics {
    pub fn input_count(&self) -> usize {
        match self {
            Self::Scan { .. }
            | Self::Filter { .. }
            | Self::Project { .. }
            | Self::HashAggregate { .. }
            | Self::InMemoryComparisonSort { .. }
            | Self::TopK { .. }
            | Self::HashDeduplicate { .. }
            | Self::InMemoryOrderedWindow { .. }
            | Self::Limit { .. }
            | Self::PassThrough { .. } => 1,
            Self::HashJoin { .. } => 2,
            Self::Concat { inputs, .. } => inputs.len(),
        }
    }

    pub fn input(&self, index: usize) -> Option<EdgeStatistics> {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => (index == 0).then_some(edges.input),
            Self::Filter { edges }
            | Self::Project { edges }
            | Self::InMemoryComparisonSort { edges }
            | Self::TopK { edges }
            | Self::InMemoryOrderedWindow { edges }
            | Self::Limit { edges }
            | Self::PassThrough { edges } => (index == 0).then_some(edges.input),
            Self::HashJoin { edges } => edges.inputs.get(index).copied(),
            Self::Concat { inputs, .. } => inputs.get(index).copied(),
        }
    }

    pub fn output(&self) -> EdgeStatistics {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => edges.output,
            Self::Filter { edges }
            | Self::Project { edges }
            | Self::InMemoryComparisonSort { edges }
            | Self::TopK { edges }
            | Self::InMemoryOrderedWindow { edges }
            | Self::Limit { edges }
            | Self::PassThrough { edges } => edges.output,
            Self::HashJoin { edges } => edges.output,
            Self::Concat { output, .. } => *output,
        }
    }
}

/// Resolves physical statistics and owns their catalog/observation freshness.
/// Returning an error makes the entire candidate unavailable.
pub trait OperatorStatisticsProvider {
    fn statistics(&self, node_id: &str) -> Result<OperatorStatistics, AnalyticalCostError>;
}

impl OperatorStatisticsProvider for HashMap<String, OperatorStatistics> {
    fn statistics(&self, node_id: &str) -> Result<OperatorStatistics, AnalyticalCostError> {
        self.get(node_id)
            .cloned()
            .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(node_id.into()))
    }
}
