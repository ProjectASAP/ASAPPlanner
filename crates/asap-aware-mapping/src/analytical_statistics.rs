//! Authoritative comparison-scope and physical-statistics contracts.
//!
//! This module does not lower logical query nodes. It defines the evidence a
//! lowering or catalog provider must supply before two physical DAGs can be
//! compared by the analytical resource model.

use std::collections::HashMap;

use asap_types::pre_asap::query_expr::{InfoMatcher, Predicate, Source};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
    TimeSelection, TimestampMs,
};
use serde::{Deserialize, Serialize};

use crate::physical_resource_cost::AnalyticalCostError;

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
    /// Provider-owned stable identifier for the physical source contents,
    /// such as a catalog snapshot, table version, or object generation.
    /// This is independent of the query's event-time `as_of` value.
    pub source_snapshot_id: String,
    /// Canonical predicates copied from the bound/canonicalized query IR.
    pub predicates: Vec<Predicate>,
    /// Symbolic selectors on an info-metric source. Ordinary query scans leave
    /// this empty because their selection is represented by `predicates`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub info_matchers: Vec<InfoMatcher>,
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
        if self
            .sources
            .iter()
            .enumerate()
            .any(|(index, source)| self.sources[..index].contains(source))
        {
            return Err(AnalyticalCostError::MissingComparisonScope(
                "duplicate source coverage",
            ));
        }
        if self
            .sources
            .iter()
            .any(|source| source.source_snapshot_id.is_empty())
        {
            return Err(AnalyticalCostError::MissingComparisonScope(
                "source_snapshot_id",
            ));
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
        (
            "sources",
            raw.sources.len() == candidate.sources.len()
                && raw
                    .sources
                    .iter()
                    .all(|source| candidate.sources.contains(source)),
        ),
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
    /// Time-series shape on the same logical edges. This is edge metadata,
    /// not an operator-specific bag of optional cost parameters.
    #[serde(default)]
    pub promql: Option<PromqlUnaryEdgeStatistics>,
}

/// Input and output evidence for a binary physical operator. The input order
/// is the physical operator's left/right order and must match its DAG children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryEdgeStatistics {
    pub inputs: [EdgeStatistics; 2],
    pub output: EdgeStatistics,
    #[serde(default)]
    pub promql: Option<PromqlBinaryEdgeStatistics>,
}

/// PromQL value shape carried alongside rows/bytes on a physical edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromqlEdgeStatistics {
    pub series: u64,
    pub evaluation_steps: u64,
    pub value_kind: PromqlValueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromqlValueKind {
    Scalar,
    /// Vector/sample data. Operator semantics determine whether rows are
    /// instant results or decoded source samples for a range evaluation.
    Vector,
    RangeVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromqlUnaryEdgeStatistics {
    pub input: PromqlEdgeStatistics,
    pub output: PromqlEdgeStatistics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromqlBinaryEdgeStatistics {
    pub inputs: [PromqlEdgeStatistics; 2],
    pub output: PromqlEdgeStatistics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromqlNaryEdgeStatistics {
    pub inputs: Vec<PromqlEdgeStatistics>,
    pub output: PromqlEdgeStatistics,
}

/// Input distribution for an algorithm that independently orders partitions.
/// The checked sum of these edges must equal the operator input. A global sort
/// has exactly one partition; an empty input has no partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionStatistics {
    pub partitions: Vec<EdgeStatistics>,
}

/// Workload-dependent evidence for one operator in an already-lowered
/// physical DAG. [`PhysicalOperator`](crate::physical_resource_cost::PhysicalOperator)
/// is the authoritative operator vocabulary: every one of its variants has a
/// matching statistics variant here.
///
/// This enum intentionally does not mirror either logical IR. `QueryExpr` and
/// `SummaryExpr` are inputs to physical lowering, and one logical node may
/// expand into several physical nodes or choose among several algorithms.
/// Physical configuration such as a Top-K limit or hash-join build side lives
/// on `PhysicalOperator`; this enum contains only workload/catalog evidence
/// required to cost the selected algorithm. Structuring that evidence by
/// physical kind prevents unrelated facts from being combined in a flat bag
/// of `Option`s.
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
        input_partitioning: PartitionStatistics,
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
        #[serde(default)]
        promql: Option<PromqlNaryEdgeStatistics>,
    },
    InMemoryAnalyticWindow {
        edges: UnaryEdgeStatistics,
        input_partitioning: PartitionStatistics,
    },
    Limit {
        edges: UnaryEdgeStatistics,
    },
    PassThrough {
        edges: UnaryEdgeStatistics,
    },
    PromqlRange {
        edges: UnaryEdgeStatistics,
        max_window_samples_per_series: u64,
    },
    PromqlSubquery {
        edges: UnaryEdgeStatistics,
        subquery_steps: u64,
    },
    PromqlBinary {
        edges: BinaryEdgeStatistics,
        matching_key_bytes: u64,
    },
    PromqlRelabel {
        edges: UnaryEdgeStatistics,
    },
    PromqlInfoEnrich {
        edges: BinaryEdgeStatistics,
        matching_key_bytes: u64,
    },
    PromqlSeriesSample {
        edges: UnaryEdgeStatistics,
        group_count: u64,
        key_bytes: u64,
    },
    PromqlScalarToVector {
        edges: UnaryEdgeStatistics,
    },
    PromqlVectorToScalar {
        edges: UnaryEdgeStatistics,
    },
    PromqlScalarLeaf {
        output: EdgeStatistics,
        promql_output: PromqlEdgeStatistics,
    },
    PromqlPerSeries {
        edges: UnaryEdgeStatistics,
        accumulator_bytes_per_series: u64,
    },
    PromqlPresence {
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
            | Self::InMemoryAnalyticWindow { .. }
            | Self::Limit { .. }
            | Self::PassThrough { .. }
            | Self::PromqlRange { .. }
            | Self::PromqlSubquery { .. }
            | Self::PromqlRelabel { .. }
            | Self::PromqlSeriesSample { .. }
            | Self::PromqlScalarToVector { .. }
            | Self::PromqlVectorToScalar { .. }
            | Self::PromqlPerSeries { .. }
            | Self::PromqlPresence { .. } => 1,
            Self::HashJoin { .. } | Self::PromqlBinary { .. } | Self::PromqlInfoEnrich { .. } => 2,
            Self::Concat { inputs, .. } => inputs.len(),
            Self::PromqlScalarLeaf { .. } => 0,
        }
    }

    pub fn input(&self, index: usize) -> Option<EdgeStatistics> {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => (index == 0).then_some(edges.input),
            Self::Filter { edges }
            | Self::Project { edges }
            | Self::InMemoryComparisonSort { edges, .. }
            | Self::TopK { edges }
            | Self::InMemoryAnalyticWindow { edges, .. }
            | Self::Limit { edges }
            | Self::PassThrough { edges }
            | Self::PromqlRange { edges, .. }
            | Self::PromqlSubquery { edges, .. }
            | Self::PromqlRelabel { edges }
            | Self::PromqlSeriesSample { edges, .. }
            | Self::PromqlScalarToVector { edges }
            | Self::PromqlVectorToScalar { edges }
            | Self::PromqlPerSeries { edges, .. }
            | Self::PromqlPresence { edges } => (index == 0).then_some(edges.input),
            Self::HashJoin { edges }
            | Self::PromqlBinary { edges, .. }
            | Self::PromqlInfoEnrich { edges, .. } => edges.inputs.get(index).copied(),
            Self::Concat { inputs, .. } => inputs.get(index).copied(),
            Self::PromqlScalarLeaf { .. } => None,
        }
    }

    pub fn output(&self) -> EdgeStatistics {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => edges.output,
            Self::Filter { edges }
            | Self::Project { edges }
            | Self::InMemoryComparisonSort { edges, .. }
            | Self::TopK { edges }
            | Self::InMemoryAnalyticWindow { edges, .. }
            | Self::Limit { edges }
            | Self::PassThrough { edges }
            | Self::PromqlRange { edges, .. }
            | Self::PromqlSubquery { edges, .. }
            | Self::PromqlRelabel { edges }
            | Self::PromqlSeriesSample { edges, .. }
            | Self::PromqlScalarToVector { edges }
            | Self::PromqlVectorToScalar { edges }
            | Self::PromqlPerSeries { edges, .. }
            | Self::PromqlPresence { edges } => edges.output,
            Self::HashJoin { edges }
            | Self::PromqlBinary { edges, .. }
            | Self::PromqlInfoEnrich { edges, .. } => edges.output,
            Self::Concat { output, .. } => *output,
            Self::PromqlScalarLeaf { output, .. } => *output,
        }
    }

    pub fn promql_input(&self, index: usize) -> Option<PromqlEdgeStatistics> {
        match self {
            Self::Concat {
                promql: Some(edges),
                ..
            } => edges.inputs.get(index).copied(),
            Self::PromqlScalarLeaf { .. } => None,
            Self::HashJoin { edges }
            | Self::PromqlBinary { edges, .. }
            | Self::PromqlInfoEnrich { edges, .. } => edges.promql?.inputs.get(index).copied(),
            _ => self.unary_promql().and_then(
                |edges| {
                    if index == 0 {
                        Some(edges.input)
                    } else {
                        None
                    }
                },
            ),
        }
    }

    pub fn promql_output(&self) -> Option<PromqlEdgeStatistics> {
        match self {
            Self::Concat {
                promql: Some(edges),
                ..
            } => Some(edges.output),
            Self::PromqlScalarLeaf { promql_output, .. } => Some(*promql_output),
            Self::HashJoin { edges }
            | Self::PromqlBinary { edges, .. }
            | Self::PromqlInfoEnrich { edges, .. } => Some(edges.promql?.output),
            _ => Some(self.unary_promql()?.output),
        }
    }

    pub(crate) fn unary_promql(&self) -> Option<PromqlUnaryEdgeStatistics> {
        match self {
            Self::Scan { edges, .. }
            | Self::Filter { edges }
            | Self::Project { edges }
            | Self::HashAggregate { edges, .. }
            | Self::InMemoryComparisonSort { edges, .. }
            | Self::TopK { edges }
            | Self::HashDeduplicate { edges, .. }
            | Self::InMemoryAnalyticWindow { edges, .. }
            | Self::Limit { edges }
            | Self::PassThrough { edges }
            | Self::PromqlRange { edges, .. }
            | Self::PromqlSubquery { edges, .. }
            | Self::PromqlRelabel { edges }
            | Self::PromqlSeriesSample { edges, .. }
            | Self::PromqlScalarToVector { edges }
            | Self::PromqlVectorToScalar { edges }
            | Self::PromqlPerSeries { edges, .. }
            | Self::PromqlPresence { edges } => edges.promql,
            _ => None,
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
