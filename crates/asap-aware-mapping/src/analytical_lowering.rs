//! Recursive lowering from the canonical query IR to analytical physical DAGs.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::analytical_cost::{
    validate_operator_semantics, AnalyticalCostError, ExecutionMultiplicity, HashJoinBuildSide,
    PhysicalDagNode, PhysicalOperator,
};
use crate::analytical_statistics::{
    ComparisonScope, EdgeStatistics, OperatorStatistics, OperatorStatisticsProvider, SourceCoverage,
};

/// A lowered physical DAG and the node whose output is the query result.
/// Keeping the root beside its nodes prevents callers from accidentally
/// estimating a valid node list from the wrong entry point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDag {
    pub nodes: Vec<PhysicalDagNode>,
    pub root: String,
    pub evidence: std::collections::HashMap<String, PhysicalNodeEvidence>,
}

/// Atomic evidence for one lowered physical node. The statistics contract is
/// reused unchanged; the separate buffer field is necessary because logical
/// edge bytes cannot stand in for an allocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalNodeEvidence {
    pub physical_id: String,
    pub statistics: OperatorStatistics,
    pub output_buffer_bytes: u64,
}

pub struct PhysicalNodeRequest<'a> {
    pub logical_node: &'a asap_types::pre_asap::QueryExpr,
    pub operator: PhysicalOperator,
    pub occurrence: usize,
    pub synthetic: bool,
    pub children: &'a [String],
    pub source_coverage: Option<&'a SourceCoverage>,
}

pub trait PhysicalNodeEvidenceProvider {
    fn evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError>;
}

impl<F> PhysicalNodeEvidenceProvider for F
where
    F: Fn(PhysicalNodeRequest<'_>) -> Result<PhysicalNodeEvidence, AnalyticalCostError>,
{
    fn evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
        self(request)
    }
}

impl OperatorStatisticsProvider for std::collections::HashMap<String, PhysicalNodeEvidence> {
    fn statistics(&self, node_id: &str) -> Result<OperatorStatistics, AnalyticalCostError> {
        let evidence = self
            .get(node_id)
            .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(node_id.into()))?;
        if evidence.physical_id != node_id {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "evidence map key differs from embedded physical identity",
            ));
        }
        Ok(evidence.statistics.clone())
    }
}

impl OperatorStatisticsProvider for PhysicalDag {
    fn statistics(&self, node_id: &str) -> Result<OperatorStatistics, AnalyticalCostError> {
        let evidence = self
            .evidence
            .get(node_id)
            .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(node_id.into()))?;
        let node = self.nodes.iter().find(|node| node.id == node_id).ok_or(
            AnalyticalCostError::InvalidPhysicalDag("evidence has no matching physical node"),
        )?;
        if evidence.physical_id != node_id {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "evidence map key differs from embedded physical identity",
            ));
        }
        if node.output_buffer_bytes != evidence.output_buffer_bytes {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical node buffer differs from evidence snapshot",
            ));
        }
        Ok(evidence.statistics.clone())
    }
}

/// Lower a resolved query operator DAG to the physical operators understood by
/// this cost model. The authoritative provider supplies statistics by the
/// stable physical IDs owned by that provider; missing evidence makes the
/// complete query unavailable. Scalar expressions remain part of their
/// containing operator's local cost.
pub fn lower_query_physical_dag(
    root: &Rc<asap_types::pre_asap::QueryExpr>,
    scope: &ComparisonScope,
    evidence: &dyn PhysicalNodeEvidenceProvider,
) -> Result<PhysicalDag, AnalyticalCostError> {
    use std::collections::HashMap;

    use asap_types::pre_asap::{GroupKeys, QueryExpr, SetOpKind};

    scope.validate()?;

    struct Lowerer<'a> {
        scope: &'a ComparisonScope,
        provider: &'a dyn PhysicalNodeEvidenceProvider,
        evidence: HashMap<String, PhysicalNodeEvidence>,
        next_id: usize,
        nodes: Vec<PhysicalDagNode>,
    }

    impl Lowerer<'_> {
        fn lower(&mut self, query: &QueryExpr) -> Result<String, AnalyticalCostError> {
            let occurrence = self.next_id;
            self.next_id += 1;
            self.lower_new(query, occurrence)
        }

        fn resolve(
            &self,
            query: &QueryExpr,
            operator: PhysicalOperator,
            occurrence: usize,
            synthetic: bool,
            children: &[String],
            source_coverage: Option<&SourceCoverage>,
        ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
            let evidence = self.provider.evidence(PhysicalNodeRequest {
                logical_node: query,
                operator,
                occurrence,
                synthetic,
                children,
                source_coverage,
            })?;
            if evidence.physical_id.is_empty() {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "provider returned an empty physical identity",
                ));
            }
            Ok(evidence)
        }

        fn push(
            &mut self,
            evidence: PhysicalNodeEvidence,
            operator: PhysicalOperator,
            children: Vec<String>,
            source_coverage: Option<SourceCoverage>,
        ) -> Result<String, AnalyticalCostError> {
            let id = evidence.physical_id.clone();
            let node = PhysicalDagNode {
                id: id.clone(),
                operator,
                children,
                source_coverage,
                output_buffer_bytes: evidence.output_buffer_bytes,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            };
            if let Some(existing) = self.nodes.iter().find(|existing| existing.id == id) {
                if existing != &node || self.evidence.get(&id) != Some(&evidence) {
                    return Err(AnalyticalCostError::InvalidPhysicalDag(
                        "provider reused a physical identity for conflicting evidence",
                    ));
                }
                return Ok(id);
            }
            self.nodes.push(node);
            self.evidence.insert(id.clone(), evidence);
            Ok(id)
        }

        fn lower_unary(
            &mut self,
            query: &QueryExpr,
            occurrence: usize,
            operator: PhysicalOperator,
            child: &QueryExpr,
        ) -> Result<String, AnalyticalCostError> {
            let child_id = self.lower(child)?;
            let children = vec![child_id.clone()];
            let evidence = self.resolve(query, operator, occurrence, false, &children, None)?;
            let statistics = &evidence.statistics;
            let child_statistics = self.node_statistics(&child_id)?;
            require_unary_edge(
                &evidence.physical_id,
                statistics,
                &child_id,
                child_statistics,
            )?;
            require_operator_statistics(operator, statistics)?;
            self.push(evidence, operator, children, None)
        }

        fn node_statistics(&self, id: &str) -> Result<&OperatorStatistics, AnalyticalCostError> {
            self.evidence
                .get(id)
                .map(|evidence| &evidence.statistics)
                .ok_or(AnalyticalCostError::InvalidPhysicalDag(
                    "lowered child statistics are missing",
                ))
        }

        fn lower_new(
            &mut self,
            query: &QueryExpr,
            occurrence: usize,
        ) -> Result<String, AnalyticalCostError> {
            match query {
                QueryExpr::Scan {
                    source, predicates, ..
                } => {
                    let coverage = bind_scan_coverage(
                        &format!("occurrence-{occurrence}"),
                        source,
                        predicates,
                        self.scope,
                    )?;
                    if predicates.is_empty() {
                        let evidence = self.resolve(
                            query,
                            PhysicalOperator::Scan,
                            occurrence,
                            false,
                            &[],
                            Some(&coverage),
                        )?;
                        require_statistics_shape(&evidence.physical_id, &evidence.statistics, 1)?;
                        require_scan_edges_equal(&evidence.statistics)?;
                        return self.push(evidence, PhysicalOperator::Scan, vec![], Some(coverage));
                    }
                    let scan_evidence = self.resolve(
                        query,
                        PhysicalOperator::Scan,
                        occurrence,
                        true,
                        &[],
                        Some(&coverage),
                    )?;
                    require_statistics_shape(
                        &scan_evidence.physical_id,
                        &scan_evidence.statistics,
                        1,
                    )?;
                    require_scan_edges_equal(&scan_evidence.statistics)?;
                    let scan_statistics = scan_evidence.statistics.clone();
                    let scan_id = self.push(
                        scan_evidence,
                        PhysicalOperator::Scan,
                        vec![],
                        Some(coverage),
                    )?;
                    let children = vec![scan_id.clone()];
                    let predicate_operations_per_row = predicates
                        .iter()
                        .try_fold(0_u64, |total, predicate| {
                            total
                                .checked_add(scalar_operation_count(&predicate.0)?)
                                .ok_or(AnalyticalCostError::Overflow)
                        })?
                        .max(1);
                    let filter_operator = PhysicalOperator::Filter {
                        predicate_operations_per_row,
                    };
                    let filter_evidence =
                        self.resolve(query, filter_operator, occurrence, false, &children, None)?;
                    require_unary_edge(
                        &filter_evidence.physical_id,
                        &filter_evidence.statistics,
                        &scan_id,
                        &scan_statistics,
                    )?;
                    require_operator_statistics(filter_operator, &filter_evidence.statistics)?;
                    self.push(filter_evidence, filter_operator, children, None)
                }
                QueryExpr::Filter { pred, child } => {
                    let operator = PhysicalOperator::Filter {
                        predicate_operations_per_row: scalar_operation_count(&pred.0)?.max(1),
                    };
                    self.lower_unary(query, occurrence, operator, child)
                }
                QueryExpr::Project { cols, child, .. } => {
                    let expression_operations_per_row = cols
                        .iter()
                        .try_fold(0_u64, |total, item| {
                            total
                                .checked_add(
                                    scalar_operation_count(&item.expr)?
                                        .checked_add(1)
                                        .ok_or(AnalyticalCostError::Overflow)?,
                                )
                                .ok_or(AnalyticalCostError::Overflow)
                        })?
                        .max(1);
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::Project {
                            expression_operations_per_row,
                        },
                        child,
                    )
                }
                QueryExpr::Aggregate {
                    reduction,
                    measures,
                    having,
                    child,
                    ..
                } => {
                    let asap_types::pre_asap::Reduction::Reduce(grouping) = reduction else {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    };
                    if grouping.is_without()
                        || having.is_some()
                        || !supports_hash_aggregate(reduction, measures)
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::HashAggregate {
                            grouping_key_count: u64::try_from(grouping.keys().len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            accumulator_count: u64::try_from(measures.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                        },
                        child,
                    )
                }
                QueryExpr::Dedup { cols, child } => {
                    let key_count = if cols.is_empty() {
                        child
                            .output_schema()
                            .map_err(|_| AnalyticalCostError::UnsupportedQueryOperator)?
                            .columns
                            .len()
                    } else {
                        cols.len()
                    };
                    if key_count == 0 {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::HashDeduplicate {
                            key_count: u64::try_from(key_count)
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                        },
                        child,
                    )
                }
                QueryExpr::Sort {
                    keys,
                    partition_by,
                    child,
                    ..
                } => {
                    if keys.is_empty() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::InMemoryComparisonSort {
                            ordering_key_count: u64::try_from(keys.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            partitioned: partition_by != &GroupKeys::none(),
                        },
                        child,
                    )
                }
                QueryExpr::Limit { n, offset, child } => {
                    if let QueryExpr::Sort {
                        keys,
                        partition_by,
                        child: sorted_child,
                        ..
                    } = child.as_ref()
                    {
                        if !keys.is_empty() && partition_by == &GroupKeys::none() {
                            let child_id = self.lower(sorted_child)?;
                            let children = vec![child_id.clone()];
                            let limit =
                                u64::try_from(*n).map_err(|_| AnalyticalCostError::Overflow)?;
                            let offset = u64::try_from(*offset)
                                .map_err(|_| AnalyticalCostError::Overflow)?;
                            let operator = PhysicalOperator::TopK {
                                limit,
                                offset,
                                ordering_key_count: u64::try_from(keys.len())
                                    .map_err(|_| AnalyticalCostError::Overflow)?,
                            };
                            let evidence =
                                self.resolve(query, operator, occurrence, false, &children, None)?;
                            let statistics = &evidence.statistics;
                            let child_statistics = self.node_statistics(&child_id)?;
                            require_unary_edge(
                                &evidence.physical_id,
                                statistics,
                                &child_id,
                                child_statistics,
                            )?;
                            let bound = limit
                                .checked_add(offset)
                                .ok_or(AnalyticalCostError::Overflow)?;
                            if bound == 0 {
                                return Err(AnalyticalCostError::MissingOrZero("topk_k"));
                            }
                            require_operator_statistics(operator, statistics)?;
                            return self.push(evidence, operator, children, None);
                        }
                    }
                    let child_id = self.lower(child)?;
                    let children = vec![child_id.clone()];
                    let operator = PhysicalOperator::Limit {
                        limit: u64::try_from(*n).map_err(|_| AnalyticalCostError::Overflow)?,
                        offset: u64::try_from(*offset)
                            .map_err(|_| AnalyticalCostError::Overflow)?,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    let statistics = &evidence.statistics;
                    let child_statistics = self.node_statistics(&child_id)?;
                    require_unary_edge(
                        &evidence.physical_id,
                        statistics,
                        &child_id,
                        child_statistics,
                    )?;
                    require_operator_statistics(operator, statistics)?;
                    self.push(evidence, operator, children, None)
                }
                QueryExpr::SQLWindowFunc {
                    func,
                    partition_by,
                    order_by,
                    child,
                    ..
                } => {
                    if order_by.is_empty()
                        || !matches!(
                            func,
                            asap_types::pre_asap::WindowFuncKind::RowNumber
                                | asap_types::pre_asap::WindowFuncKind::Rank
                                | asap_types::pre_asap::WindowFuncKind::DenseRank
                        )
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(
                        query,
                        occurrence,
                        PhysicalOperator::InMemoryAnalyticWindow {
                            partition_key_count: u64::try_from(partition_by.keys().len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            ordering_key_count: u64::try_from(order_by.len())
                                .map_err(|_| AnalyticalCostError::Overflow)?,
                            function_operations_per_row: 1,
                        },
                        child,
                    )
                }
                QueryExpr::TimeShift { shift, child } => {
                    if !shift.is_identity() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(query, occurrence, PhysicalOperator::PassThrough, child)
                }
                QueryExpr::Concat { children } => {
                    let child_ids = children
                        .iter()
                        .map(|child| self.lower(child))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.lower_concat(query, occurrence, child_ids)
                }
                QueryExpr::SetOp {
                    kind: SetOpKind::Union,
                    all: true,
                    left,
                    right,
                } => {
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    self.lower_concat(query, occurrence, vec![left_id, right_id])
                }
                QueryExpr::Join {
                    kind,
                    pred,
                    left,
                    right,
                } => {
                    let equality_key_count =
                        if matches!(kind, asap_types::pre_asap::JoinKind::Cross) {
                            None
                        } else {
                            hash_join_key_count(&pred.0, left, right)
                        };
                    let Some(equality_key_count) = equality_key_count else {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    };
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    let children = vec![left_id.clone(), right_id.clone()];
                    let left_statistics = self.node_statistics(&left_id)?;
                    let right_statistics = self.node_statistics(&right_id)?;
                    let build_side =
                        if left_statistics.output().bytes <= right_statistics.output().bytes {
                            HashJoinBuildSide::Left
                        } else {
                            HashJoinBuildSide::Right
                        };
                    let operator = PhysicalOperator::HashJoin {
                        build_side,
                        equality_key_count,
                    };
                    let evidence =
                        self.resolve(query, operator, occurrence, false, &children, None)?;
                    let statistics = &evidence.statistics;
                    require_statistics_shape(&evidence.physical_id, statistics, 2)?;
                    if statistics.input(0) != Some(left_statistics.output())
                        || statistics.input(1) != Some(right_statistics.output())
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "join inputs do not match child outputs",
                        ));
                    }
                    require_operator_statistics(operator, statistics)?;
                    self.push(evidence, operator, children, None)
                }
                _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
            }
        }

        fn lower_concat(
            &mut self,
            query: &QueryExpr,
            occurrence: usize,
            child_ids: Vec<String>,
        ) -> Result<String, AnalyticalCostError> {
            if child_ids.is_empty() {
                return Err(AnalyticalCostError::InvalidPhysicalDag(
                    "concat has no children",
                ));
            }
            let evidence = self.resolve(
                query,
                PhysicalOperator::Concat,
                occurrence,
                false,
                &child_ids,
                None,
            )?;
            let statistics = &evidence.statistics;
            require_statistics_shape(&evidence.physical_id, statistics, child_ids.len())?;
            let (rows, bytes) = child_ids.iter().enumerate().try_fold(
                (0_u64, 0_u64),
                |(rows, bytes), (index, child)| {
                    let child_statistics = self.node_statistics(child)?;
                    if statistics.input(index) != Some(child_statistics.output()) {
                        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                            parent: evidence.physical_id.clone(),
                            child: child.clone(),
                            input_index: index,
                        });
                    }
                    Ok::<_, AnalyticalCostError>((
                        rows.checked_add(child_statistics.output().rows)
                            .ok_or(AnalyticalCostError::Overflow)?,
                        bytes
                            .checked_add(child_statistics.output().bytes)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    ))
                },
            )?;
            if statistics.output() != (EdgeStatistics { rows, bytes }) {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "concat statistics do not equal the sum of child outputs",
                ));
            }
            require_operator_statistics(PhysicalOperator::Concat, statistics)?;
            self.push(evidence, PhysicalOperator::Concat, child_ids, None)
        }
    }

    let mut lowerer = Lowerer {
        scope,
        provider: evidence,
        evidence: HashMap::new(),
        next_id: 0,
        nodes: Vec::new(),
    };
    let root = lowerer.lower(root)?;
    validate_source_consumption(&lowerer.nodes, scope)?;
    Ok(PhysicalDag {
        nodes: lowerer.nodes,
        root,
        evidence: lowerer.evidence,
    })
}

fn validate_source_consumption(
    nodes: &[PhysicalDagNode],
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let consumed = nodes
        .iter()
        .filter(|node| matches!(node.operator, PhysicalOperator::Scan))
        .filter_map(|node| node.source_coverage.as_ref())
        .collect::<Vec<_>>();
    for coverage in &consumed {
        if !scope.sources.contains(coverage) {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical scan consumes a source outside the comparison scope",
            ));
        }
    }
    if scope
        .sources
        .iter()
        .any(|expected| !consumed.contains(&expected))
    {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "physical scans omit a comparison-scope source",
        ));
    }
    Ok(())
}

fn require_statistics_shape(
    node: &str,
    statistics: &OperatorStatistics,
    input_count: usize,
) -> Result<(), AnalyticalCostError> {
    if statistics.input_count() != input_count {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.into(),
            reason: "wrong input-edge count",
        });
    }
    if (0..statistics.input_count())
        .filter_map(|index| statistics.input(index))
        .chain(std::iter::once(statistics.output()))
        .any(|edge| !edge.is_consistent())
    {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.into(),
            reason: "edge rows and logical bytes are inconsistent",
        });
    }
    Ok(())
}

fn require_scan_edges_equal(statistics: &OperatorStatistics) -> Result<(), AnalyticalCostError> {
    if statistics.input(0) != Some(statistics.output()) {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "Scan external input edge does not match its output edge",
        ));
    }
    Ok(())
}

fn require_unary_edge(
    node: &str,
    statistics: &OperatorStatistics,
    child_id: &str,
    child: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    require_statistics_shape(node, statistics, 1)?;
    if statistics.input(0) != Some(child.output()) {
        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
            parent: node.into(),
            child: child_id.into(),
            input_index: 0,
        });
    }
    Ok(())
}

fn require_operator_statistics(
    operator: PhysicalOperator,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    validate_operator_semantics(operator, statistics)
}

fn bind_scan_coverage(
    node_id: &str,
    source: &asap_types::pre_asap::Source,
    predicates: &[asap_types::pre_asap::Predicate],
    scope: &ComparisonScope,
) -> Result<SourceCoverage, AnalyticalCostError> {
    let mut matches = scope
        .sources
        .iter()
        .filter(|coverage| coverage.source == *source && coverage.predicates == predicates);
    let coverage = matches
        .next()
        .cloned()
        .ok_or_else(|| AnalyticalCostError::ScanOutsideComparisonScope(node_id.into()))?;
    if matches.any(|candidate| candidate != &coverage) {
        return Err(AnalyticalCostError::InvalidPhysicalDag(
            "scan source coverage is ambiguous",
        ));
    }
    Ok(coverage)
}

fn hash_join_key_count(
    expr: &asap_types::pre_asap::QueryExpr,
    left: &asap_types::pre_asap::QueryExpr,
    right: &asap_types::pre_asap::QueryExpr,
) -> Option<u64> {
    use asap_types::pre_asap::{CompareOpKind, QueryExpr};

    let (Ok(left_schema), Ok(right_schema)) = (left.output_schema(), right.output_schema()) else {
        return None;
    };
    let left_width = left_schema.columns.len();
    let total_width = left_width.saturating_add(right_schema.columns.len());

    fn column_side(column: usize, left_width: usize, total_width: usize) -> Option<bool> {
        if column < left_width {
            Some(false)
        } else if column < total_width {
            Some(true)
        } else {
            None
        }
    }

    fn predicate(expr: &QueryExpr, left_width: usize, total_width: usize) -> Option<u64> {
        match expr {
            QueryExpr::Compare {
                left,
                op: CompareOpKind::Eq,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (QueryExpr::Column(left), QueryExpr::Column(right)) => match (
                    column_side(*left, left_width, total_width),
                    column_side(*right, left_width, total_width),
                ) {
                    (Some(false), Some(true)) | (Some(true), Some(false)) => Some(1),
                    _ => None,
                },
                _ => None,
            },
            QueryExpr::BoolAnd(parts) if !parts.is_empty() => {
                parts.iter().try_fold(0_u64, |count, part| {
                    count.checked_add(predicate(part, left_width, total_width)?)
                })
            }
            _ => None,
        }
    }

    predicate(expr, left_width, total_width)
}

fn scalar_operation_count(
    expr: &asap_types::pre_asap::QueryExpr,
) -> Result<u64, AnalyticalCostError> {
    use asap_types::pre_asap::QueryExpr;

    let add = |parts: &[&QueryExpr]| {
        parts.iter().try_fold(0_u64, |total, part| {
            total
                .checked_add(scalar_operation_count(part)?)
                .ok_or(AnalyticalCostError::Overflow)
        })
    };
    let with_local = |children| {
        add(children)?
            .checked_add(1)
            .ok_or(AnalyticalCostError::Overflow)
    };
    match expr {
        QueryExpr::Column(_)
        | QueryExpr::Literal(_)
        | QueryExpr::EvalTimestamp
        | QueryExpr::CurrentTimestamp => Ok(0),
        QueryExpr::Compare { left, right, .. } | QueryExpr::Arithmetic { left, right, .. } => {
            with_local(&[left, right])
        }
        QueryExpr::BoolAnd(parts) | QueryExpr::BoolOr(parts) => {
            let children = parts.iter().collect::<Vec<_>>();
            add(&children)?
                .checked_add(
                    u64::try_from(parts.len().saturating_sub(1))
                        .map_err(|_| AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)
        }
        QueryExpr::Not(child)
        | QueryExpr::IsNull(child)
        | QueryExpr::IsNotNull(child)
        | QueryExpr::PromqlScalarBridge(child) => with_local(&[child]),
        QueryExpr::Cast { expr, .. } => with_local(&[expr]),
        QueryExpr::InList { expr, list, .. } => {
            let mut children = Vec::with_capacity(list.len() + 1);
            children.push(expr.as_ref());
            children.extend(list.iter());
            add(&children)?
                .checked_add(u64::try_from(list.len()).map_err(|_| AnalyticalCostError::Overflow)?)
                .ok_or(AnalyticalCostError::Overflow)
        }
        QueryExpr::FunctionCall { args, .. } => {
            let children = args.iter().collect::<Vec<_>>();
            with_local(&children)
        }
        QueryExpr::Case {
            operand,
            branches,
            else_expr,
        } => {
            let mut children = Vec::new();
            if let Some(operand) = operand {
                children.push(operand.as_ref());
            }
            for (when, then) in branches {
                children.extend([when, then]);
            }
            if let Some(else_expr) = else_expr {
                children.push(else_expr.as_ref());
            }
            with_local(&children)
        }
        _ => Err(AnalyticalCostError::UnsupportedQueryOperator),
    }
}

fn supports_hash_aggregate(
    reduction: &asap_types::pre_asap::Reduction,
    measures: &[asap_types::pre_asap::AggIntent],
) -> bool {
    use asap_types::pre_asap::{AggIntent, Reduction};

    matches!(reduction, Reduction::Reduce(_))
        && !measures.is_empty()
        && measures.iter().all(|intent| {
            matches!(
                intent,
                AggIntent::Count { .. }
                    | AggIntent::Sum { .. }
                    | AggIntent::Min { .. }
                    | AggIntent::Max { .. }
                    | AggIntent::Avg { .. }
                    | AggIntent::StdDev { .. }
                    | AggIntent::Variance { .. }
                    | AggIntent::Group
                    | AggIntent::CountValues { .. }
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytical_cost::{
        estimate_physical_dag, estimate_physical_dag_comparison, PhysicalDagEstimateRequest,
    };
    use crate::analytical_statistics::{
        validate_comparison_scopes, BinaryEdgeStatistics, PartitionStatistics, UnaryEdgeStatistics,
    };
    use asap_types::workload::{
        DataArrival, DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
    };
    use std::collections::HashMap;

    fn edge(rows: u64, bytes: u64) -> EdgeStatistics {
        EdgeStatistics { rows, bytes }
    }

    fn unary_edges(input: EdgeStatistics, output: EdgeStatistics) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics { input, output }
    }

    fn scan_stats(edge: EdgeStatistics, source_read_bytes: u64) -> OperatorStatistics {
        OperatorStatistics::Scan {
            edges: unary_edges(edge, edge),
            source_read_bytes,
        }
    }

    fn unary_statistics(
        operator: PhysicalOperator,
        input: EdgeStatistics,
        output: EdgeStatistics,
    ) -> OperatorStatistics {
        let edges = unary_edges(input, output);
        match operator {
            PhysicalOperator::Filter { .. } => OperatorStatistics::Filter { edges },
            PhysicalOperator::Project { .. } => OperatorStatistics::Project { edges },
            PhysicalOperator::InMemoryComparisonSort { .. } => {
                OperatorStatistics::InMemoryComparisonSort {
                    edges,
                    input_partitioning: PartitionStatistics {
                        partitions: (!input.eq(&edge(0, 0)))
                            .then_some(input)
                            .into_iter()
                            .collect(),
                    },
                }
            }
            PhysicalOperator::TopK { .. } => OperatorStatistics::TopK { edges },
            PhysicalOperator::InMemoryAnalyticWindow { .. } => {
                OperatorStatistics::InMemoryAnalyticWindow {
                    edges,
                    input_partitioning: PartitionStatistics {
                        partitions: (!input.eq(&edge(0, 0)))
                            .then_some(input)
                            .into_iter()
                            .collect(),
                    },
                }
            }
            PhysicalOperator::Limit { .. } => OperatorStatistics::Limit { edges },
            PhysicalOperator::PassThrough => OperatorStatistics::PassThrough { edges },
            _ => panic!("test helper requires a stateless unary operator"),
        }
    }

    fn evidence(statistics: OperatorStatistics) -> PhysicalNodeEvidence {
        PhysicalNodeEvidence {
            physical_id: String::new(),
            output_buffer_bytes: statistics.output().bytes.min(1_024),
            statistics,
        }
    }

    fn scripted<'a>(
        provided: &'a HashMap<String, PhysicalNodeEvidence>,
    ) -> impl Fn(PhysicalNodeRequest<'_>) -> Result<PhysicalNodeEvidence, AnalyticalCostError> + 'a
    {
        move |request| {
            let key = if request.synthetic {
                format!("query-{}-scan", request.occurrence)
            } else {
                format!("query-{}", request.occurrence)
            };
            let mut evidence = provided
                .get(&key)
                .cloned()
                .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(key.clone()))?;
            evidence.physical_id = key;
            Ok(evidence)
        }
    }

    fn scope(sources: Vec<SourceCoverage>) -> ComparisonScope {
        ComparisonScope {
            data_arrival: DataArrival::AtRest,
            planning_time: TimestampMs(1_000),
            horizon: DurationMs(1_000),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope: QueryTimeScope::Longitudinal,
                lookback: Some(DurationMs(1_000)),
                as_of: Some(TimestampMs(1_000)),
            },
            sources,
        }
    }

    fn coverage(
        source: asap_types::pre_asap::Source,
        predicates: Vec<asap_types::pre_asap::Predicate>,
    ) -> SourceCoverage {
        SourceCoverage {
            source,
            source_snapshot_id: "snapshot-1".into(),
            predicates,
        }
    }

    #[test]
    fn query_lowering_recurses_and_fuses_global_sort_limit() {
        use asap_types::pre_asap::{AggIntent, GroupKeys, QueryExpr, Reduction, SortKey, Source};
        use asap_types::pre_asap::{Column, DataType, Schema};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![asap_types::pre_asap::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::ScalarValue::Boolean(true)),
            ))],
            schema: Schema::new(vec![
                Column::new("service", DataType::Utf8, false),
                Column::new("value", DataType::Float64, false),
            ]),
        });
        let aggregate = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec![],
            having: None,
            child: Rc::clone(&scan),
        });
        let sort = Rc::new(QueryExpr::Sort {
            keys: vec![SortKey {
                expr: QueryExpr::Column(0),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: GroupKeys::none(),
            child: aggregate,
        });
        let root = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 5,
            child: sort,
        });

        let scan_coverage = coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![asap_types::pre_asap::Predicate(Rc::new(
                QueryExpr::Literal(asap_types::pre_asap::ScalarValue::Boolean(true)),
            ))],
        );
        let scope = scope(vec![scan_coverage]);
        let aggregate_statistics = OperatorStatistics::HashAggregate {
            edges: unary_edges(edge(400, 25_600), edge(100, 4_000)),
            group_count: 100,
            key_bytes: 16,
            accumulator_bytes_per_group: 8,
        };
        let topk_statistics = unary_statistics(
            PhysicalOperator::TopK {
                limit: 10,
                offset: 5,
                ordering_key_count: 1,
            },
            edge(100, 4_000),
            edge(10, 400),
        );
        let raw_scan = scan_stats(edge(1_000, 64_000), 64_000);
        let provided = HashMap::from([
            ("query-2-scan".into(), evidence(raw_scan)),
            (
                "query-2".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(1_000, 64_000),
                    edge(400, 25_600),
                )),
            ),
            ("query-1".into(), evidence(aggregate_statistics)),
            ("query-0".into(), evidence(topk_statistics)),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter {
                    predicate_operations_per_row: 1,
                },
                PhysicalOperator::HashAggregate {
                    grouping_key_count: 1,
                    accumulator_count: 1,
                },
                PhysicalOperator::TopK {
                    limit: 10,
                    offset: 5,
                    ordering_key_count: 1,
                },
            ]
        );
        let topk = dag.nodes.last().unwrap();
        assert_eq!(topk.children, vec![dag.nodes[2].id.clone()]);
        assert!(matches!(
            provided[&topk.id].statistics,
            OperatorStatistics::TopK { .. }
        ));
        let physical_scan = &dag.nodes[0];
        assert_eq!(physical_scan.id, "query-2-scan");
        assert_eq!(
            physical_scan.source_coverage,
            Some(scope.sources[0].clone())
        );
        assert_eq!(physical_scan.output_buffer_bytes, 1_024);
        assert_ne!(
            physical_scan.output_buffer_bytes,
            provided[&physical_scan.id].statistics.output().bytes
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());

        let mut inconsistent_scan = provided.clone();
        inconsistent_scan
            .get_mut("query-2-scan")
            .unwrap()
            .statistics = OperatorStatistics::Scan {
            edges: unary_edges(edge(1_000, 64_000), edge(999, 63_936)),
            source_read_bytes: 64_000,
        };
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&inconsistent_scan)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "Scan external input edge does not match its output edge"
            ))
        );
    }

    #[test]
    fn query_lowering_shares_only_provider_identified_physical_nodes() {
        use asap_types::pre_asap::{Column, CompareOpKind, DataType, Schema};
        use asap_types::pre_asap::{JoinKind, Predicate, QueryExpr, Source};
        use std::rc::Rc;

        let shared = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "dimensions".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let root = Rc::new(QueryExpr::Join {
            kind: JoinKind::Inner,
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Rc::new(QueryExpr::Column(1)),
            })),
            left: Rc::clone(&shared),
            right: Rc::clone(&shared),
        });
        let source_coverage = coverage(
            Source::Table {
                table_ref: "dimensions".into(),
            },
            vec![],
        );
        let independent_scope = scope(vec![source_coverage.clone()]);
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let join_statistics = OperatorStatistics::HashJoin {
            edges: BinaryEdgeStatistics {
                inputs: [edge(100, 800), edge(100, 800)],
                output: edge(25, 400),
            },
        };
        let provided = HashMap::from([
            ("query-1".into(), evidence(scan_statistics.clone())),
            ("query-2".into(), evidence(scan_statistics.clone())),
            ("query-0".into(), evidence(join_statistics.clone())),
        ]);
        let dag =
            lower_query_physical_dag(&root, &independent_scope, &scripted(&provided)).unwrap();

        assert_eq!(dag.nodes.len(), 3);
        assert_ne!(dag.nodes[2].children[0], dag.nodes[2].children[1]);
        let estimate =
            estimate_physical_dag(&dag.nodes, &dag.root, &independent_scope, &dag.evidence)
                .unwrap();
        assert_eq!(estimate.scan_bytes, 1_600);

        let shared_provider = |request: PhysicalNodeRequest<'_>| {
            let (physical_id, statistics) = match request.operator {
                PhysicalOperator::Scan => ("shared-scan", scan_statistics.clone()),
                PhysicalOperator::HashJoin { .. } => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output().bytes.min(1_024),
                statistics,
            })
        };
        let shared_scope = scope(vec![source_coverage]);
        let shared_dag = lower_query_physical_dag(&root, &shared_scope, &shared_provider).unwrap();
        assert_eq!(shared_dag.nodes.len(), 2);
        assert_eq!(
            shared_dag.nodes[1].children,
            vec!["shared-scan".to_owned(); 2]
        );
        let comparison = estimate_physical_dag_comparison(
            PhysicalDagEstimateRequest {
                nodes: &dag.nodes,
                root: &dag.root,
                scope: &independent_scope,
                statistics: &dag,
            },
            PhysicalDagEstimateRequest {
                nodes: &shared_dag.nodes,
                root: &shared_dag.root,
                scope: &shared_scope,
                statistics: &shared_dag,
            },
        )
        .unwrap();
        assert_eq!(comparison.raw.scan_bytes, 1_600);
        assert_eq!(comparison.candidate.scan_bytes, 800);

        let mut drifted_buffer = shared_dag.clone();
        drifted_buffer.nodes[0].output_buffer_bytes += 1;
        assert_eq!(
            estimate_physical_dag(
                &drifted_buffer.nodes,
                &drifted_buffer.root,
                &shared_scope,
                &drifted_buffer,
            ),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical node buffer differs from evidence snapshot"
            ))
        );
        let mut drifted_identity = shared_dag.clone();
        drifted_identity
            .evidence
            .get_mut("shared-scan")
            .unwrap()
            .physical_id = "different".into();
        assert_eq!(
            estimate_physical_dag(
                &drifted_identity.nodes,
                &drifted_identity.root,
                &shared_scope,
                &drifted_identity,
            ),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "evidence map key differs from embedded physical identity"
            ))
        );

        let conflicting_identity = |request: PhysicalNodeRequest<'_>| {
            let (physical_id, mut statistics) = match request.operator {
                PhysicalOperator::Scan => ("shared-scan", scan_statistics.clone()),
                PhysicalOperator::HashJoin { .. } => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            if request.operator == PhysicalOperator::Scan && request.occurrence == 2 {
                statistics = scan_stats(edge(100, 800), 801);
            }
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output().bytes.min(1_024),
                statistics,
            })
        };
        assert_eq!(
            lower_query_physical_dag(&root, &shared_scope, &conflicting_identity),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "provider reused a physical identity for conflicting evidence"
            ))
        );

        let invalid = Rc::new(QueryExpr::Join {
            kind: JoinKind::Inner,
            pred: Predicate(Rc::new(QueryExpr::Compare {
                left: Rc::new(QueryExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Rc::new(QueryExpr::Column(0)),
            })),
            left: Rc::clone(&shared),
            right: Rc::clone(&shared),
        });
        assert_eq!(
            lower_query_physical_dag(&invalid, &shared_scope, &shared_provider),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn query_lowering_covers_relational_unary_operators() {
        use asap_types::pre_asap::{Column, DataType, ScalarValue, Schema};
        use asap_types::pre_asap::{
            GroupKeys, Predicate, QueryExpr, SortKey, Source, TimeShift, WindowFuncKind,
        };
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let filter = Rc::new(QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: scan,
        });
        let project = Rc::new(QueryExpr::Project {
            cols: vec![],
            qualifier: None,
            child: filter,
        });
        let dedup = Rc::new(QueryExpr::Dedup {
            cols: vec![0],
            child: project,
        });
        let window = Rc::new(QueryExpr::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::none(),
            order_by: vec![SortKey {
                expr: QueryExpr::Column(0),
                ascending: true,
                nulls_first: false,
            }],
            frame: None,
            output_name: "rn".into(),
            child: dedup,
        });
        let sort = Rc::new(QueryExpr::Sort {
            keys: vec![SortKey {
                expr: QueryExpr::Column(0),
                ascending: true,
                nulls_first: false,
            }],
            partition_by: GroupKeys::by(vec![0]),
            child: window,
        });
        let limit = Rc::new(QueryExpr::Limit {
            n: 20,
            offset: 0,
            child: sort,
        });
        let root = Rc::new(QueryExpr::TimeShift {
            shift: TimeShift::default(),
            child: limit,
        });

        let source_coverage = coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        );
        let scope = scope(vec![source_coverage]);
        let scan_statistics = scan_stats(edge(1_000, 8_000), 8_000);
        let dedup_statistics = OperatorStatistics::HashDeduplicate {
            edges: unary_edges(edge(800, 3_200), edge(500, 2_000)),
            distinct_key_count: 500,
            key_bytes: 8,
        };
        let provided = HashMap::from([
            ("query-7".into(), evidence(scan_statistics)),
            (
                "query-6".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(1_000, 8_000),
                    edge(800, 6_400),
                )),
            ),
            (
                "query-5".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Project {
                        expression_operations_per_row: 1,
                    },
                    edge(800, 6_400),
                    edge(800, 3_200),
                )),
            ),
            ("query-4".into(), evidence(dedup_statistics)),
            (
                "query-3".into(),
                evidence(unary_statistics(
                    PhysicalOperator::InMemoryAnalyticWindow {
                        partition_key_count: 0,
                        ordering_key_count: 1,
                        function_operations_per_row: 1,
                    },
                    edge(500, 2_000),
                    edge(500, 6_000),
                )),
            ),
            (
                "query-2".into(),
                evidence(unary_statistics(
                    PhysicalOperator::InMemoryComparisonSort {
                        ordering_key_count: 1,
                        partitioned: true,
                    },
                    edge(500, 6_000),
                    edge(500, 6_000),
                )),
            ),
            (
                "query-1".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Limit {
                        limit: 20,
                        offset: 0,
                    },
                    edge(500, 6_000),
                    edge(20, 240),
                )),
            ),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::PassThrough,
                    edge(20, 240),
                    edge(20, 240),
                )),
            ),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();

        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::Filter {
                    predicate_operations_per_row: 1,
                },
                PhysicalOperator::Project {
                    expression_operations_per_row: 1,
                },
                PhysicalOperator::HashDeduplicate { key_count: 1 },
                PhysicalOperator::InMemoryAnalyticWindow {
                    partition_key_count: 0,
                    ordering_key_count: 1,
                    function_operations_per_row: 1,
                },
                PhysicalOperator::InMemoryComparisonSort {
                    ordering_key_count: 1,
                    partitioned: true,
                },
                PhysicalOperator::Limit {
                    limit: 20,
                    offset: 0,
                },
                PhysicalOperator::PassThrough,
            ]
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());
    }

    #[test]
    fn query_lowering_maps_concat_and_union_all_but_rejects_distinct_set_ops() {
        use asap_types::pre_asap::{Column, DataType, Schema};
        use asap_types::pre_asap::{QueryExpr, SetOpKind, Source};
        use std::rc::Rc;

        let scan = |name: &str| QueryExpr::Scan {
            source: Source::Table {
                table_ref: name.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        };
        let union = Rc::new(QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: true,
            left: Rc::new(scan("a")),
            right: Rc::new(scan("b")),
        });
        let scope = scope(vec![
            coverage(
                Source::Table {
                    table_ref: "a".into(),
                },
                vec![],
            ),
            coverage(
                Source::Table {
                    table_ref: "b".into(),
                },
                vec![],
            ),
        ]);
        let left_statistics = scan_stats(edge(10, 80), 80);
        let right_statistics = scan_stats(edge(20, 160), 160);
        let provided = HashMap::from([
            ("query-1".into(), evidence(left_statistics)),
            ("query-2".into(), evidence(right_statistics)),
            (
                "query-0".into(),
                evidence(OperatorStatistics::Concat {
                    inputs: vec![edge(10, 80), edge(20, 160)],
                    output: edge(30, 240),
                }),
            ),
        ]);
        let dag = lower_query_physical_dag(&union, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let mut reversed_scope = scope.clone();
        reversed_scope.sources.reverse();
        assert_eq!(validate_comparison_scopes(&scope, &reversed_scope), Ok(1));
        let mut duplicate_scope = scope.clone();
        duplicate_scope.sources.push(scope.sources[0].clone());
        assert_eq!(
            duplicate_scope.validate(),
            Err(AnalyticalCostError::MissingComparisonScope(
                "duplicate source coverage"
            ))
        );

        let concat = Rc::new(QueryExpr::Concat {
            children: vec![scan("a"), scan("b")],
        });
        let dag = lower_query_physical_dag(&concat, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes.last().unwrap().operator, PhysicalOperator::Concat);

        let distinct_union = Rc::new(QueryExpr::SetOp {
            kind: SetOpKind::Union,
            all: false,
            left: Rc::new(scan("a")),
            right: Rc::new(scan("b")),
        });
        assert_eq!(
            lower_query_physical_dag(&distinct_union, &scope, &scripted(&provided)),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn query_lowering_fails_closed_for_missing_or_inconsistent_statistics() {
        use asap_types::pre_asap::{Column, DataType, Schema};
        use asap_types::pre_asap::{QueryExpr, Source};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let root = Rc::new(QueryExpr::Project {
            cols: vec![],
            qualifier: None,
            child: scan,
        });

        let comparison_scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let missing = HashMap::<String, PhysicalNodeEvidence>::new();
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &scripted(&missing)),
            Err(AnalyticalCostError::MissingOperatorStatistics(
                "query-1".into()
            ))
        );
        struct MissingBuffer;
        impl PhysicalNodeEvidenceProvider for MissingBuffer {
            fn evidence(
                &self,
                _request: PhysicalNodeRequest<'_>,
            ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
                Err(AnalyticalCostError::MissingOrStale("output_buffer_bytes"))
            }
        }
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &MissingBuffer),
            Err(AnalyticalCostError::MissingOrStale("output_buffer_bytes"))
        );
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let conflicting = HashMap::from([
            ("query-1".into(), evidence(scan_statistics)),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Project {
                        expression_operations_per_row: 1,
                    },
                    edge(99, 792),
                    edge(99, 396),
                )),
            ),
        ]);
        assert_eq!(
            lower_query_physical_dag(&root, &comparison_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::ConflictingEdgeStatistics {
                parent: "query-0".into(),
                child: "query-1".into(),
                input_index: 0,
            })
        );

        let outside_scope = scope(vec![coverage(
            Source::Table {
                table_ref: "other".into(),
            },
            vec![],
        )]);
        assert_eq!(
            lower_query_physical_dag(&root, &outside_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::ScanOutsideComparisonScope(
                "occurrence-1".into()
            ))
        );

        let mut second_snapshot = comparison_scope.sources[0].clone();
        second_snapshot.source_snapshot_id = "snapshot-2".into();
        let ambiguous_scope = scope(vec![comparison_scope.sources[0].clone(), second_snapshot]);
        assert_eq!(
            lower_query_physical_dag(&root, &ambiguous_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "scan source coverage is ambiguous"
            ))
        );

        let mut complete = conflicting.clone();
        complete.get_mut("query-0").unwrap().statistics = unary_statistics(
            PhysicalOperator::Project {
                expression_operations_per_row: 1,
            },
            edge(100, 800),
            edge(100, 400),
        );
        let extra_scope = scope(vec![
            comparison_scope.sources[0].clone(),
            coverage(
                Source::Table {
                    table_ref: "unused".into(),
                },
                vec![],
            ),
        ]);
        assert_eq!(
            lower_query_physical_dag(&root, &extra_scope, &scripted(&complete)),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "physical scans omit a comparison-scope source"
            ))
        );
    }

    #[test]
    fn query_lowering_accepts_a_consistently_empty_edge() {
        use asap_types::pre_asap::{Column, DataType, ScalarValue, Schema};
        use asap_types::pre_asap::{Predicate, QueryExpr, Source};
        use std::rc::Rc;

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("id", DataType::Int64, false)]),
        });
        let filter = Rc::new(QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(false)))),
            child: scan,
        });
        let root = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 0,
            child: filter,
        });

        let scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let scan_statistics = scan_stats(edge(100, 800), 800);
        let provided = HashMap::from([
            ("query-2".into(), evidence(scan_statistics)),
            (
                "query-1".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Filter {
                        predicate_operations_per_row: 1,
                    },
                    edge(100, 800),
                    edge(0, 0),
                )),
            ),
            (
                "query-0".into(),
                evidence(unary_statistics(
                    PhysicalOperator::Limit {
                        limit: 10,
                        offset: 0,
                    },
                    edge(0, 0),
                    edge(0, 0),
                )),
            ),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).unwrap();
        assert_eq!(estimate.cpu_ops, 200.0);
        assert_eq!(estimate.scan_bytes, 800);
    }

    #[test]
    fn query_lowering_rejects_aggregates_without_a_hash_implementation() {
        use asap_types::pre_asap::{
            AggIntent, GroupKeys, QueryExpr, Reduction, Source, WindowFuncKind,
        };
        use asap_types::pre_asap::{Column, DataType, Schema};
        use asap_types::types::AccuracyTarget;
        use std::rc::Rc;

        let scan = || {
            Rc::new(QueryExpr::Scan {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
            })
        };
        let exact_quantile = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Quantile {
                col: Some(0),
                q: 0.99,
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: scan(),
        });
        let per_entity = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: scan(),
        });
        let empty_sort_limit = Rc::new(QueryExpr::Limit {
            n: 10,
            offset: 0,
            child: Rc::new(QueryExpr::Sort {
                keys: vec![],
                partition_by: GroupKeys::none(),
                child: scan(),
            }),
        });
        let unsupported_window = Rc::new(QueryExpr::SQLWindowFunc {
            func: WindowFuncKind::Lag,
            args: vec![QueryExpr::Column(0)],
            partition_by: GroupKeys::none(),
            order_by: vec![],
            frame: None,
            output_name: "lag".into(),
            child: scan(),
        });
        let shifted = Rc::new(QueryExpr::TimeShift {
            shift: asap_types::pre_asap::TimeShift {
                offset_ms: 60_000,
                at: None,
            },
            child: scan(),
        });
        let scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let unavailable = HashMap::<String, PhysicalNodeEvidence>::new();
        for query in [
            &exact_quantile,
            &per_entity,
            &empty_sort_limit,
            &unsupported_window,
            &shifted,
        ] {
            assert_eq!(
                lower_query_physical_dag(query, &scope, &scripted(&unavailable)),
                Err(AnalyticalCostError::UnsupportedQueryOperator)
            );
        }
    }

    #[test]
    fn scalar_work_counts_every_local_predicate_operation() {
        use asap_types::pre_asap::{CompareOpKind, QueryExpr, ScalarValue};

        let comparison = || QueryExpr::Compare {
            left: Rc::new(QueryExpr::Column(0)),
            op: CompareOpKind::Eq,
            right: Rc::new(QueryExpr::Literal(ScalarValue::Int64(1))),
        };
        let predicate = QueryExpr::BoolAnd(vec![comparison(), comparison()]);

        assert_eq!(scalar_operation_count(&predicate), Ok(3));
    }
}
