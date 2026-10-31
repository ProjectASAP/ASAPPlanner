//! Physical-operator statistics contracts.
//!
//! This module does not lower logical query nodes. It defines the per-operator
//! statistics the analytical resource model prices an operator from.

use serde::{Deserialize, Serialize};

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
pub(crate) struct UnaryEdgeStatistics {
    pub input: EdgeStatistics,
    pub output: EdgeStatistics,
    /// Time-series shape on the same logical edges. This is edge metadata,
    /// not an operator-specific bag of optional cost parameters.
    #[serde(default)]
    pub promql: Option<PromqlUnaryEdgeStatistics>,
}

/// Evidence for one scalar input a `Filter` or `Project` expression reads
/// beside its row input: the edge from a `PromqlVectorToScalar` child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScalarInputStatistics {
    pub(crate) edge: EdgeStatistics,
    #[serde(default)]
    pub(crate) promql: Option<PromqlEdgeStatistics>,
}

/// Input and output evidence for a binary physical operator. The input order
/// is the physical operator's left/right order and must match its DAG children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BinaryEdgeStatistics {
    pub(crate) inputs: [EdgeStatistics; 2],
    pub(crate) output: EdgeStatistics,
    #[serde(default)]
    pub(crate) promql: Option<PromqlBinaryEdgeStatistics>,
}

/// PromQL value shape carried alongside rows/bytes on a physical edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromqlEdgeStatistics {
    pub(crate) series: u64,
    pub(crate) evaluation_steps: u64,
    pub(crate) value_kind: PromqlValueKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromqlValueKind {
    Scalar,
    /// Vector/sample data. Operator semantics determine whether rows are
    /// instant results or decoded source samples for a range evaluation.
    Vector,
    RangeVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromqlUnaryEdgeStatistics {
    pub(crate) input: PromqlEdgeStatistics,
    pub(crate) output: PromqlEdgeStatistics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromqlBinaryEdgeStatistics {
    pub(crate) inputs: [PromqlEdgeStatistics; 2],
    pub(crate) output: PromqlEdgeStatistics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromqlNaryEdgeStatistics {
    pub(crate) inputs: Vec<PromqlEdgeStatistics>,
    pub(crate) output: PromqlEdgeStatistics,
}

/// Input distribution for an algorithm that independently orders partitions.
/// The checked sum of these edges must equal the operator input. A global sort
/// has exactly one partition; an empty input has no partitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PartitionStatistics {
    pub partitions: Vec<EdgeStatistics>,
}

/// Workload-dependent evidence for one operator in an already-lowered
/// physical DAG. [`PhysicalOperator`](crate::analytical::PhysicalOperator)
/// is the authoritative operator vocabulary: every one of its variants has a
/// matching statistics variant here.
///
/// This enum intentionally does not mirror the logical IR. `OperatorNode`s
/// are inputs to physical lowering, and one logical node may
/// expand into several physical nodes or choose among several algorithms.
/// Physical configuration such as a Top-K limit or hash-join build side lives
/// on `PhysicalOperator`; this enum contains only workload/catalog evidence
/// required to cost the selected algorithm. Structuring that evidence by
/// physical kind prevents unrelated facts from being combined in a flat bag
/// of `Option`s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operator", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OperatorStatistics {
    Scan {
        edges: UnaryEdgeStatistics,
        /// Physical bytes read from storage, independent of decoded logical
        /// bytes on the source edge.
        source_read_bytes: u64,
    },
    Filter {
        edges: UnaryEdgeStatistics,
        /// One input per operator reference (`scalar(v)`) the predicate
        /// reads, after the row input, in DAG child order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scalar_inputs: Vec<ScalarInputStatistics>,
    },
    Project {
        edges: UnaryEdgeStatistics,
        /// One input per operator reference (`scalar(v)`) the expressions
        /// read, after the row input, in DAG child order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scalar_inputs: Vec<ScalarInputStatistics>,
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
    pub(crate) fn input(&self, index: usize) -> Option<EdgeStatistics> {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => (index == 0).then_some(edges.input),
            Self::Filter {
                edges,
                scalar_inputs,
            }
            | Self::Project {
                edges,
                scalar_inputs,
            } => match index {
                0 => Some(edges.input),
                _ => scalar_inputs.get(index - 1).map(|input| input.edge),
            },
            Self::InMemoryComparisonSort { edges, .. }
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

    pub(crate) fn output(&self) -> EdgeStatistics {
        match self {
            Self::Scan { edges, .. }
            | Self::HashAggregate { edges, .. }
            | Self::HashDeduplicate { edges, .. } => edges.output,
            Self::Filter { edges, .. }
            | Self::Project { edges, .. }
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

    pub(crate) fn promql_output(&self) -> Option<PromqlEdgeStatistics> {
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
            | Self::Filter { edges, .. }
            | Self::Project { edges, .. }
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
