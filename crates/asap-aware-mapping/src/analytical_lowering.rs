//! Recursive lowering from the canonical query IR to analytical physical DAGs.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::analytical_cost::{
    validate_operator_semantics as require_operator_statistics, AnalyticalCostError,
    ExecutionMultiplicity, PhysicalDagNode, PhysicalOperator,
};
use crate::analytical_statistics::{
    ComparisonScope, EdgeStatistics, OperatorStatistics, OperatorStatisticsProvider,
    PromqlBinaryOperandMode, SourceCoverage,
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
/// deterministic physical IDs assigned here; missing evidence makes the
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
            require_promql_unary_edge(operator, statistics, child_statistics)?;
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
                    let filter_evidence = self.resolve(
                        query,
                        PhysicalOperator::Filter,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    require_unary_edge(
                        &filter_evidence.physical_id,
                        &filter_evidence.statistics,
                        &scan_id,
                        &scan_statistics,
                    )?;
                    require_operator_statistics(
                        PhysicalOperator::Filter,
                        &filter_evidence.statistics,
                    )?;
                    self.push(filter_evidence, PhysicalOperator::Filter, children, None)
                }
                QueryExpr::Filter { child, .. } => {
                    self.lower_unary(query, occurrence, PhysicalOperator::Filter, child)
                }
                QueryExpr::Project { child, .. } => {
                    self.lower_unary(query, occurrence, PhysicalOperator::Project, child)
                }
                QueryExpr::Aggregate {
                    reduction,
                    measures,
                    having,
                    child,
                    ..
                } => {
                    if having.is_some() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    let operator = aggregate_operator(reduction, measures)
                        .ok_or(AnalyticalCostError::UnsupportedQueryOperator)?;
                    self.lower_unary(query, occurrence, operator, child)
                }
                QueryExpr::Dedup { child, .. } => {
                    self.lower_unary(query, occurrence, PhysicalOperator::Deduplicate, child)
                }
                QueryExpr::Sort { keys, child, .. } => {
                    if keys.is_empty() {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    self.lower_unary(query, occurrence, PhysicalOperator::Sort, child)
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
                            let evidence = self.resolve(
                                query,
                                PhysicalOperator::TopK,
                                occurrence,
                                false,
                                &children,
                                None,
                            )?;
                            let statistics = &evidence.statistics;
                            let child_statistics = self.node_statistics(&child_id)?;
                            require_unary_edge(
                                &evidence.physical_id,
                                statistics,
                                &child_id,
                                child_statistics,
                            )?;
                            require_promql_unary_edge(
                                PhysicalOperator::TopK,
                                statistics,
                                child_statistics,
                            )?;
                            let bound = n
                                .checked_add(*offset)
                                .and_then(|value| u64::try_from(value).ok())
                                .ok_or(AnalyticalCostError::Overflow)?;
                            if bound == 0 {
                                return Err(AnalyticalCostError::MissingOrZero("topk_k"));
                            }
                            if statistics.k != Some(bound) {
                                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                                    "Top-K statistics disagree with LIMIT n + offset",
                                ));
                            }
                            require_limit_cardinality(*n, *offset, statistics)?;
                            return self.push(evidence, PhysicalOperator::TopK, children, None);
                        }
                    }
                    let child_id = self.lower(child)?;
                    let children = vec![child_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::Limit,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    let child_statistics = self.node_statistics(&child_id)?;
                    require_unary_edge(
                        &evidence.physical_id,
                        statistics,
                        &child_id,
                        child_statistics,
                    )?;
                    require_promql_unary_edge(
                        PhysicalOperator::Limit,
                        statistics,
                        child_statistics,
                    )?;
                    require_operator_statistics(PhysicalOperator::Limit, statistics)?;
                    require_limit_cardinality(*n, *offset, statistics)?;
                    require_limit_consumption(*n, *offset, statistics)?;
                    self.push(evidence, PhysicalOperator::Limit, children, None)
                }
                QueryExpr::SQLWindowFunc {
                    func,
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
                    self.lower_unary(query, occurrence, PhysicalOperator::Window, child)
                }
                QueryExpr::TimeRange { range, child } => {
                    if range.is_zero() {
                        return Err(AnalyticalCostError::MissingOrZero("range"));
                    }
                    self.lower_unary(query, occurrence, PhysicalOperator::PromqlRange, child)
                }
                QueryExpr::PromqlSubquery {
                    range,
                    resolution,
                    child,
                } => {
                    let child_id = self.lower(child)?;
                    let children = vec![child_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlSubquery,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    let child_statistics = self.node_statistics(&child_id)?;
                    require_unary_edge(
                        &evidence.physical_id,
                        statistics,
                        &child_id,
                        child_statistics,
                    )?;
                    require_promql_unary_edge(
                        PhysicalOperator::PromqlSubquery,
                        statistics,
                        child_statistics,
                    )?;
                    require_operator_statistics(PhysicalOperator::PromqlSubquery, statistics)?;
                    let supplied = require_promql_statistics(statistics, 1)?
                        .subquery_steps
                        .ok_or(AnalyticalCostError::MissingOrZero("subquery_steps"))?;
                    let parent_steps = require_promql_statistics(statistics, 1)?.evaluation_steps;
                    let expected_child_steps = parent_steps
                        .checked_mul(supplied)
                        .ok_or(AnalyticalCostError::Overflow)?;
                    let child_steps = child_statistics
                        .promql
                        .as_ref()
                        .ok_or(AnalyticalCostError::MissingOrStale(
                            "child_promql_operator_statistics",
                        ))?
                        .evaluation_steps;
                    if child_steps != expected_child_steps {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "subquery child steps do not equal parent steps times subquery steps",
                        ));
                    }
                    if let Some(expected) = subquery_steps(*range, *resolution)? {
                        if supplied != expected {
                            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                                "subquery_steps disagree with range and resolution",
                            ));
                        }
                    }
                    self.push(evidence, PhysicalOperator::PromqlSubquery, children, None)
                }
                QueryExpr::TimeShift { child, .. } => {
                    self.lower_unary(query, occurrence, PhysicalOperator::PassThrough, child)
                }
                QueryExpr::PromqlRelabel { child, .. } => {
                    self.lower_unary(query, occurrence, PhysicalOperator::PromqlRelabel, child)
                }
                QueryExpr::PromqlSeriesSample {
                    by, kind, child, ..
                } => {
                    let child_id = self.lower(child)?;
                    let children = vec![child_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlSeriesSample,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    let child_statistics = self.node_statistics(&child_id)?;
                    require_unary_edge(
                        &evidence.physical_id,
                        statistics,
                        &child_id,
                        child_statistics,
                    )?;
                    require_promql_unary_edge(
                        PhysicalOperator::PromqlSeriesSample,
                        statistics,
                        child_statistics,
                    )?;
                    validate_series_sample(by, *kind, statistics)?;
                    require_operator_statistics(PhysicalOperator::PromqlSeriesSample, statistics)?;
                    self.push(
                        evidence,
                        PhysicalOperator::PromqlSeriesSample,
                        children,
                        None,
                    )
                }
                QueryExpr::PromqlInfoEnrich { selector, child } => {
                    let left_id = self.lower(child)?;
                    let coverage = bind_info_coverage(
                        &format!("occurrence-{occurrence}-info"),
                        selector,
                        self.scope,
                    )?;
                    let info_evidence = self.resolve(
                        query,
                        PhysicalOperator::Scan,
                        occurrence,
                        true,
                        &[],
                        Some(&coverage),
                    )?;
                    require_statistics_shape(
                        &info_evidence.physical_id,
                        &info_evidence.statistics,
                        1,
                    )?;
                    require_scan_edges_equal(&info_evidence.statistics)?;
                    let info_statistics = info_evidence.statistics.clone();
                    let info_id = self.push(
                        info_evidence,
                        PhysicalOperator::Scan,
                        vec![],
                        Some(coverage),
                    )?;
                    let children = vec![left_id.clone(), info_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlInfoEnrich,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    require_statistics_shape(&evidence.physical_id, statistics, 2)?;
                    if statistics.inputs[0] != self.node_statistics(&left_id)?.output
                        || statistics.inputs[1] != info_statistics.output
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "info enrichment inputs do not match child outputs",
                        ));
                    }
                    require_promql_binary_edges(
                        statistics,
                        self.node_statistics(&left_id)?,
                        &info_statistics,
                    )?;
                    if selector.iter().any(|matcher| matcher.label != "__name__")
                        && !matches!(
                            require_promql_statistics(statistics, 2)?.scalar_ops_per_row,
                            Some(value) if value > 0
                        )
                    {
                        return Err(AnalyticalCostError::MissingOrZero(
                            "info_matcher_ops_per_row",
                        ));
                    }
                    require_operator_statistics(PhysicalOperator::PromqlInfoEnrich, statistics)?;
                    self.push(evidence, PhysicalOperator::PromqlInfoEnrich, children, None)
                }
                QueryExpr::BinaryOp {
                    lhs,
                    rhs,
                    vector_match,
                    ..
                } => {
                    let left_scalar = is_promql_scalar(lhs);
                    let right_scalar = is_promql_scalar(rhs);
                    if left_scalar && right_scalar {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    let left_id = self.lower(lhs)?;
                    let right_id = self.lower(rhs)?;
                    let children = vec![left_id.clone(), right_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlVectorBinary,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    require_statistics_shape(&evidence.physical_id, statistics, 2)?;
                    if statistics.inputs[0] != self.node_statistics(&left_id)?.output
                        || statistics.inputs[1] != self.node_statistics(&right_id)?.output
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "PromQL binary inputs do not match child outputs",
                        ));
                    }
                    let operand_mode = match (left_scalar, right_scalar) {
                        (false, false) => PromqlBinaryOperandMode::VectorVector,
                        (false, true) => PromqlBinaryOperandMode::VectorScalar,
                        (true, false) => PromqlBinaryOperandMode::ScalarVector,
                        (true, true) => unreachable!("scalar/scalar was rejected above"),
                    };
                    if require_promql_statistics(statistics, 2)?.binary_operand_mode
                        != Some(operand_mode)
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "PromQL binary operand mode disagrees with its logical operands",
                        ));
                    }
                    if left_scalar || right_scalar {
                        if vector_match.is_some()
                            || statistics.hash_join_build_side.is_some()
                            || statistics.key_bytes.is_some()
                        {
                            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                                "scalar/vector binary operation cannot have label-match state",
                            ));
                        }
                    } else if statistics.hash_join_build_side.is_none()
                        || statistics.key_bytes.is_none()
                    {
                        return Err(AnalyticalCostError::MissingOrStale(
                            "vector_binary_label_match_statistics",
                        ));
                    }
                    require_promql_binary_edges(
                        statistics,
                        self.node_statistics(&left_id)?,
                        self.node_statistics(&right_id)?,
                    )?;
                    require_operator_statistics(PhysicalOperator::PromqlVectorBinary, statistics)?;
                    self.push(
                        evidence,
                        PhysicalOperator::PromqlVectorBinary,
                        children,
                        None,
                    )
                }
                QueryExpr::PromqlVectorFromScalar(child) => self.lower_unary(
                    query,
                    occurrence,
                    PhysicalOperator::PromqlScalarToVector,
                    child,
                ),
                QueryExpr::PromqlScalarFromVector(child) => self.lower_unary(
                    query,
                    occurrence,
                    PhysicalOperator::PromqlVectorToScalar,
                    child,
                ),
                QueryExpr::PromqlScalarBridge(inner)
                    if matches!(
                        inner.as_ref(),
                        QueryExpr::Literal(asap_types::pre_asap::ScalarValue::Float64(_))
                    ) =>
                {
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlScalarLeaf,
                        occurrence,
                        false,
                        &[],
                        None,
                    )?;
                    require_statistics_shape(&evidence.physical_id, &evidence.statistics, 0)?;
                    require_operator_statistics(
                        PhysicalOperator::PromqlScalarLeaf,
                        &evidence.statistics,
                    )?;
                    self.push(evidence, PhysicalOperator::PromqlScalarLeaf, vec![], None)
                }
                QueryExpr::EvalTimestamp => {
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::PromqlScalarLeaf,
                        occurrence,
                        false,
                        &[],
                        None,
                    )?;
                    require_statistics_shape(&evidence.physical_id, &evidence.statistics, 0)?;
                    require_operator_statistics(
                        PhysicalOperator::PromqlScalarLeaf,
                        &evidence.statistics,
                    )?;
                    self.push(evidence, PhysicalOperator::PromqlScalarLeaf, vec![], None)
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
                    if matches!(kind, asap_types::pre_asap::JoinKind::Cross)
                        || !is_hash_join_predicate(&pred.0, left, right)
                    {
                        return Err(AnalyticalCostError::UnsupportedQueryOperator);
                    }
                    let left_id = self.lower(left)?;
                    let right_id = self.lower(right)?;
                    let children = vec![left_id.clone(), right_id.clone()];
                    let evidence = self.resolve(
                        query,
                        PhysicalOperator::HashJoin,
                        occurrence,
                        false,
                        &children,
                        None,
                    )?;
                    let statistics = &evidence.statistics;
                    require_statistics_shape(&evidence.physical_id, statistics, 2)?;
                    let left_statistics = self.node_statistics(&left_id)?;
                    let right_statistics = self.node_statistics(&right_id)?;
                    if statistics.inputs[0] != left_statistics.output
                        || statistics.inputs[1] != right_statistics.output
                    {
                        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                            "join inputs do not match child outputs",
                        ));
                    }
                    require_operator_statistics(PhysicalOperator::HashJoin, statistics)?;
                    self.push(evidence, PhysicalOperator::HashJoin, children, None)
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
                    if statistics.inputs[index] != child_statistics.output {
                        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
                            parent: evidence.physical_id.clone(),
                            child: child.clone(),
                            input_index: index,
                        });
                    }
                    Ok::<_, AnalyticalCostError>((
                        rows.checked_add(child_statistics.output.rows)
                            .ok_or(AnalyticalCostError::Overflow)?,
                        bytes
                            .checked_add(child_statistics.output.bytes)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    ))
                },
            )?;
            if statistics.output != (EdgeStatistics { rows, bytes }) {
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
    if statistics.inputs.len() != input_count {
        return Err(AnalyticalCostError::InvalidOperatorStatistics {
            node: node.into(),
            reason: "wrong input-edge count",
        });
    }
    if statistics
        .inputs
        .iter()
        .chain(std::iter::once(&statistics.output))
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
    if statistics.inputs[0] != statistics.output {
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
    if statistics.inputs[0] != child.output {
        return Err(AnalyticalCostError::ConflictingEdgeStatistics {
            parent: node.into(),
            child: child_id.into(),
            input_index: 0,
        });
    }
    Ok(())
}

fn require_promql_statistics(
    statistics: &OperatorStatistics,
    arity: usize,
) -> Result<&crate::analytical_statistics::PromqlOperatorStatistics, AnalyticalCostError> {
    let promql = statistics
        .promql
        .as_ref()
        .ok_or(AnalyticalCostError::MissingOrStale(
            "promql_operator_statistics",
        ))?;
    if promql.evaluation_steps == 0 {
        return Err(AnalyticalCostError::MissingOrZero("evaluation_steps"));
    }
    if promql.input_series.len() != arity {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL input-series arity does not match physical inputs",
        ));
    }
    for (index, series) in promql.input_series.iter().enumerate() {
        if *series > statistics.inputs[index].rows && statistics.inputs[index].rows > 0 {
            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "input series exceed input rows",
            ));
        }
    }
    if promql.output_series > statistics.output.rows && statistics.output.rows > 0 {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "output series exceed output rows",
        ));
    }
    Ok(promql)
}

fn require_promql_binary_edges(
    statistics: &OperatorStatistics,
    left: &OperatorStatistics,
    right: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let parent = require_promql_statistics(statistics, 2)?;
    let left = left
        .promql
        .as_ref()
        .ok_or(AnalyticalCostError::MissingOrStale(
            "left_promql_operator_statistics",
        ))?;
    let right = right
        .promql
        .as_ref()
        .ok_or(AnalyticalCostError::MissingOrStale(
            "right_promql_operator_statistics",
        ))?;
    if parent.input_series != [left.output_series, right.output_series]
        || parent.evaluation_steps != left.evaluation_steps
        || parent.evaluation_steps != right.evaluation_steps
    {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL binary series or step statistics do not match its children",
        ));
    }
    Ok(())
}

fn require_promql_unary_edge(
    operator: PhysicalOperator,
    statistics: &OperatorStatistics,
    child: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    match (statistics.promql.as_ref(), child.promql.as_ref()) {
        (None, None) => Ok(()),
        (Some(parent), Some(child)) => {
            require_promql_statistics(statistics, 1)?;
            if parent.input_series[0] != child.output_series
                || (!matches!(operator, PhysicalOperator::PromqlSubquery)
                    && parent.evaluation_steps != child.evaluation_steps)
            {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "PromQL unary series or step statistics do not match its child",
                ));
            }
            Ok(())
        }
        _ => Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "PromQL statistics are missing on one side of an edge",
        )),
    }
}

fn subquery_steps(
    range: std::time::Duration,
    resolution: Option<std::time::Duration>,
) -> Result<Option<u64>, AnalyticalCostError> {
    if range.is_zero() {
        return Err(AnalyticalCostError::MissingOrZero("subquery range"));
    }
    let Some(resolution) = resolution else {
        return Ok(None);
    };
    if resolution.is_zero() {
        return Err(AnalyticalCostError::MissingOrZero("subquery resolution"));
    }
    Ok(Some(
        u64::try_from(range.as_nanos() / resolution.as_nanos() + 1)
            .map_err(|_| AnalyticalCostError::Overflow)?,
    ))
}

fn validate_series_sample(
    by: &asap_types::pre_asap::GroupKeys,
    kind: asap_types::pre_asap::SampleKind,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let promql = require_promql_statistics(statistics, 1)?;
    let groups = if !by.is_without() && by.keys().is_empty() {
        1
    } else {
        statistics
            .group_count
            .ok_or(AnalyticalCostError::MissingOrStale("sample_group_count"))?
    };
    match kind {
        asap_types::pre_asap::SampleKind::LimitK(k) => {
            let k = u64::try_from(k).map_err(|_| AnalyticalCostError::Overflow)?;
            if statistics.k != Some(k) {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "limitk statistics disagree with query k",
                ));
            }
            let bound = groups.checked_mul(k).ok_or(AnalyticalCostError::Overflow)?;
            if promql.output_series > promql.input_series[0].min(bound) {
                return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                    "limitk output exceeds its cardinality bound",
                ));
            }
        }
        asap_types::pre_asap::SampleKind::LimitRatio(ratio)
            if !ratio.is_finite() || !(-1.0..=1.0).contains(&ratio) =>
        {
            return Err(AnalyticalCostError::UnsupportedQueryOperator)
        }
        asap_types::pre_asap::SampleKind::LimitRatio(_) => {}
    }
    Ok(())
}

fn require_limit_cardinality(
    n: usize,
    offset: usize,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let n = u64::try_from(n).map_err(|_| AnalyticalCostError::Overflow)?;
    let offset = u64::try_from(offset).map_err(|_| AnalyticalCostError::Overflow)?;
    let expected = statistics.inputs[0].rows.saturating_sub(offset).min(n);
    if statistics.output.rows != expected {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "limit output does not match n and offset",
        ));
    }
    Ok(())
}

fn require_limit_consumption(
    n: usize,
    offset: usize,
    statistics: &OperatorStatistics,
) -> Result<(), AnalyticalCostError> {
    let n = u64::try_from(n).map_err(|_| AnalyticalCostError::Overflow)?;
    let offset = u64::try_from(offset).map_err(|_| AnalyticalCostError::Overflow)?;
    let expected_consumed = if n == 0 {
        0
    } else {
        statistics.inputs[0]
            .rows
            .min(offset.checked_add(n).ok_or(AnalyticalCostError::Overflow)?)
    };
    if statistics.limit_rows_consumed != Some(expected_consumed) {
        return Err(AnalyticalCostError::InconsistentOperatorStatistics(
            "limit rows consumed do not match n and offset",
        ));
    }
    Ok(())
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

fn bind_info_coverage(
    node_id: &str,
    selector: &[asap_types::pre_asap::InfoMatcher],
    scope: &ComparisonScope,
) -> Result<SourceCoverage, AnalyticalCostError> {
    use asap_types::pre_asap::{CompareOpKind, Source};

    let mut metric: Option<&str> = None;
    for matcher in selector
        .iter()
        .filter(|matcher| matcher.label == "__name__")
    {
        if matcher.op != CompareOpKind::Eq {
            return Err(AnalyticalCostError::UnsupportedQueryOperator);
        }
        if metric.is_some_and(|current| current != matcher.value) {
            return Err(AnalyticalCostError::UnsupportedQueryOperator);
        }
        metric = Some(&matcher.value);
    }
    let source = Source::TimeSeries {
        metric: metric.unwrap_or("target_info").into(),
    };
    bind_scan_coverage(node_id, &source, &[], scope)
}

fn is_hash_join_predicate(
    expr: &asap_types::pre_asap::QueryExpr,
    left: &asap_types::pre_asap::QueryExpr,
    right: &asap_types::pre_asap::QueryExpr,
) -> bool {
    use asap_types::pre_asap::{CompareOpKind, QueryExpr};

    let (Ok(left_schema), Ok(right_schema)) = (left.output_schema(), right.output_schema()) else {
        return false;
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

    fn predicate(expr: &QueryExpr, left_width: usize, total_width: usize) -> bool {
        match expr {
            QueryExpr::Compare {
                left,
                op: CompareOpKind::Eq,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (QueryExpr::Column(left), QueryExpr::Column(right)) => matches!(
                    (
                        column_side(*left, left_width, total_width),
                        column_side(*right, left_width, total_width)
                    ),
                    (Some(false), Some(true)) | (Some(true), Some(false))
                ),
                _ => false,
            },
            QueryExpr::BoolAnd(parts) => {
                !parts.is_empty()
                    && parts
                        .iter()
                        .all(|part| predicate(part, left_width, total_width))
            }
            _ => false,
        }
    }

    predicate(expr, left_width, total_width)
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

fn aggregate_operator(
    reduction: &asap_types::pre_asap::Reduction,
    measures: &[asap_types::pre_asap::AggIntent],
) -> Option<PhysicalOperator> {
    use asap_types::pre_asap::Reduction;

    if supports_hash_aggregate(reduction, measures) {
        Some(PhysicalOperator::HashAggregate)
    } else if matches!(reduction, Reduction::PerEntity)
        && !measures.is_empty()
        && measures.iter().all(presence_intent)
    {
        Some(PhysicalOperator::PromqlPresence)
    } else if matches!(reduction, Reduction::PerEntity)
        && !measures.is_empty()
        && measures.iter().all(fixed_state_per_series_intent)
    {
        Some(PhysicalOperator::PromqlPerSeries)
    } else {
        None
    }
}

fn presence_intent(intent: &asap_types::pre_asap::AggIntent) -> bool {
    matches!(
        intent,
        asap_types::pre_asap::AggIntent::Absent | asap_types::pre_asap::AggIntent::AbsentOverTime
    )
}

fn fixed_state_per_series_intent(intent: &asap_types::pre_asap::AggIntent) -> bool {
    use asap_types::pre_asap::AggIntent;
    matches!(
        intent,
        AggIntent::Rate
            | AggIntent::Count { .. }
            | AggIntent::Sum { .. }
            | AggIntent::Min { .. }
            | AggIntent::Max { .. }
            | AggIntent::Avg { .. }
            | AggIntent::StdDev { .. }
            | AggIntent::Variance { .. }
            | AggIntent::Increase
            | AggIntent::Changes
            | AggIntent::Delta
            | AggIntent::IDelta
            | AggIntent::Deriv
            | AggIntent::Resets
            | AggIntent::PredictLinear { .. }
            | AggIntent::DoubleExpSmoothing { .. }
            | AggIntent::HistogramCount
            | AggIntent::HistogramSum
            | AggIntent::HistogramAvg
            | AggIntent::HistogramStdDev
            | AggIntent::HistogramStdVar
            | AggIntent::HistogramFraction { .. }
            | AggIntent::Math(_)
            | AggIntent::PresentOverTime
            | AggIntent::TimeFn(_)
            | AggIntent::LastOverTime
            | AggIntent::FirstOverTime
            | AggIntent::TsOfMinOverTime
            | AggIntent::TsOfMaxOverTime
            | AggIntent::TsOfFirstOverTime
            | AggIntent::TsOfLastOverTime
    )
}

fn is_promql_scalar(query: &asap_types::pre_asap::QueryExpr) -> bool {
    use asap_types::pre_asap::QueryExpr;
    matches!(
        query,
        QueryExpr::PromqlScalarBridge(_)
            | QueryExpr::PromqlScalarFromVector(_)
            | QueryExpr::EvalTimestamp
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytical_cost::{
        estimate_physical_dag, estimate_physical_dag_comparison, HashJoinBuildSide,
        PhysicalDagEstimateRequest,
    };
    use crate::analytical_statistics::validate_comparison_scopes;
    use asap_types::workload::{
        DataArrival, DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
    };
    use std::collections::HashMap;

    fn edge(rows: u64, bytes: u64) -> EdgeStatistics {
        EdgeStatistics { rows, bytes }
    }

    fn statistics(inputs: Vec<EdgeStatistics>, output: EdgeStatistics) -> OperatorStatistics {
        OperatorStatistics {
            source_scan_bytes: 0,
            inputs,
            output,
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            limit_rows_consumed: None,
            hash_join_build_side: None,
            promql: None,
        }
    }

    fn evidence(statistics: OperatorStatistics) -> PhysicalNodeEvidence {
        PhysicalNodeEvidence {
            physical_id: String::new(),
            output_buffer_bytes: statistics.output.bytes.min(1_024),
            statistics,
        }
    }

    fn with_promql(
        mut statistics: OperatorStatistics,
        input_series: Vec<u64>,
        output_series: u64,
        evaluation_steps: u64,
    ) -> OperatorStatistics {
        statistics.promql = Some(crate::analytical_statistics::PromqlOperatorStatistics {
            input_series,
            output_series,
            evaluation_steps,
            window_samples_per_series: None,
            subquery_steps: None,
            scalar_ops_per_row: None,
            binary_operand_mode: None,
        });
        statistics
    }

    fn scripted<'a>(
        provided: &'a HashMap<String, PhysicalNodeEvidence>,
    ) -> impl Fn(PhysicalNodeRequest<'_>) -> Result<PhysicalNodeEvidence, AnalyticalCostError> + 'a
    {
        move |request| {
            let key = if request.synthetic
                && matches!(
                    request.logical_node,
                    asap_types::pre_asap::QueryExpr::PromqlInfoEnrich { .. }
                ) {
                format!("query-{}-info-scan", request.occurrence)
            } else if request.synthetic {
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
            snapshot_id: "snapshot-1".into(),
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
        let mut aggregate_statistics = statistics(vec![edge(400, 25_600)], edge(100, 4_000));
        aggregate_statistics.group_count = Some(100);
        aggregate_statistics.key_bytes = Some(16);
        aggregate_statistics.aggregate_value_bytes = Some(8);
        let mut topk_statistics = statistics(vec![edge(100, 4_000)], edge(10, 400));
        topk_statistics.k = Some(15);
        let mut raw_scan = statistics(vec![edge(1_000, 64_000)], edge(1_000, 64_000));
        raw_scan.source_scan_bytes = 64_000;
        let provided = HashMap::from([
            ("query-2-scan".into(), evidence(raw_scan)),
            (
                "query-2".into(),
                evidence(statistics(vec![edge(1_000, 64_000)], edge(400, 25_600))),
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
                PhysicalOperator::Filter,
                PhysicalOperator::HashAggregate,
                PhysicalOperator::TopK,
            ]
        );
        let topk = dag.nodes.last().unwrap();
        assert_eq!(topk.children, vec![dag.nodes[2].id.clone()]);
        assert_eq!(provided[&topk.id].statistics.k, Some(15));
        let physical_scan = &dag.nodes[0];
        assert_eq!(physical_scan.id, "query-2-scan");
        assert_eq!(
            physical_scan.source_coverage,
            Some(scope.sources[0].clone())
        );
        assert_eq!(physical_scan.output_buffer_bytes, 1_024);
        assert_ne!(
            physical_scan.output_buffer_bytes,
            provided[&physical_scan.id].statistics.output.bytes
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());

        let mut inconsistent_scan = provided.clone();
        inconsistent_scan
            .get_mut("query-2-scan")
            .unwrap()
            .statistics
            .output = edge(999, 63_936);
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
        let mut scan_statistics = statistics(vec![edge(100, 800)], edge(100, 800));
        scan_statistics.source_scan_bytes = 800;
        let mut join_statistics = statistics(vec![edge(100, 800), edge(100, 800)], edge(25, 400));
        join_statistics.hash_join_build_side = Some(HashJoinBuildSide::Right);
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
                PhysicalOperator::HashJoin => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output.bytes.min(1_024),
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
                PhysicalOperator::HashJoin => ("join", join_statistics.clone()),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            if request.operator == PhysicalOperator::Scan && request.occurrence == 2 {
                statistics.source_scan_bytes += 1;
            }
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: statistics.output.bytes.min(1_024),
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
        let mut scan_statistics = statistics(vec![edge(1_000, 8_000)], edge(1_000, 8_000));
        scan_statistics.source_scan_bytes = 8_000;
        let mut dedup_statistics = statistics(vec![edge(800, 3_200)], edge(500, 2_000));
        dedup_statistics.group_count = Some(500);
        dedup_statistics.key_bytes = Some(8);
        let provided = HashMap::from([
            ("query-7".into(), evidence(scan_statistics)),
            (
                "query-6".into(),
                evidence(statistics(vec![edge(1_000, 8_000)], edge(800, 6_400))),
            ),
            (
                "query-5".into(),
                evidence(statistics(vec![edge(800, 6_400)], edge(800, 3_200))),
            ),
            ("query-4".into(), evidence(dedup_statistics)),
            (
                "query-3".into(),
                evidence(statistics(vec![edge(500, 2_000)], edge(500, 6_000))),
            ),
            (
                "query-2".into(),
                evidence(statistics(vec![edge(500, 6_000)], edge(500, 6_000))),
            ),
            (
                "query-1".into(),
                evidence(OperatorStatistics {
                    limit_rows_consumed: Some(20),
                    ..statistics(vec![edge(500, 6_000)], edge(20, 240))
                }),
            ),
            (
                "query-0".into(),
                evidence(statistics(vec![edge(20, 240)], edge(20, 240))),
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
                PhysicalOperator::Filter,
                PhysicalOperator::Project,
                PhysicalOperator::Deduplicate,
                PhysicalOperator::Window,
                PhysicalOperator::Sort,
                PhysicalOperator::Limit,
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
        let mut left_statistics = statistics(vec![edge(10, 80)], edge(10, 80));
        left_statistics.source_scan_bytes = 80;
        let mut right_statistics = statistics(vec![edge(20, 160)], edge(20, 160));
        right_statistics.source_scan_bytes = 160;
        let provided = HashMap::from([
            ("query-1".into(), evidence(left_statistics)),
            ("query-2".into(), evidence(right_statistics)),
            (
                "query-0".into(),
                evidence(statistics(vec![edge(10, 80), edge(20, 160)], edge(30, 240))),
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
        let mut scan_statistics = statistics(vec![edge(100, 800)], edge(100, 800));
        scan_statistics.source_scan_bytes = 800;
        let conflicting = HashMap::from([
            ("query-1".into(), evidence(scan_statistics)),
            (
                "query-0".into(),
                evidence(statistics(vec![edge(99, 792)], edge(99, 396))),
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
        second_snapshot.snapshot_id = "snapshot-2".into();
        let ambiguous_scope = scope(vec![comparison_scope.sources[0].clone(), second_snapshot]);
        assert_eq!(
            lower_query_physical_dag(&root, &ambiguous_scope, &scripted(&conflicting)),
            Err(AnalyticalCostError::InvalidPhysicalDag(
                "scan source coverage is ambiguous"
            ))
        );

        let mut complete = conflicting.clone();
        complete.get_mut("query-0").unwrap().statistics =
            statistics(vec![edge(100, 800)], edge(100, 400));
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
        let mut scan_statistics = statistics(vec![edge(100, 800)], edge(100, 800));
        scan_statistics.source_scan_bytes = 800;
        let provided = HashMap::from([
            ("query-2".into(), evidence(scan_statistics)),
            (
                "query-1".into(),
                evidence(statistics(vec![edge(100, 800)], edge(0, 0))),
            ),
            (
                "query-0".into(),
                evidence(OperatorStatistics {
                    limit_rows_consumed: Some(0),
                    ..statistics(vec![edge(0, 0)], edge(0, 0))
                }),
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
        let scope = scope(vec![coverage(
            Source::Table {
                table_ref: "events".into(),
            },
            vec![],
        )]);
        let unavailable = HashMap::<String, PhysicalNodeEvidence>::new();
        assert_eq!(
            lower_query_physical_dag(&exact_quantile, &scope, &scripted(&unavailable)),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
        for query in [&empty_sort_limit, &unsupported_window] {
            assert_eq!(
                lower_query_physical_dag(query, &scope, &scripted(&unavailable)),
                Err(AnalyticalCostError::UnsupportedQueryOperator)
            );
        }
    }

    #[test]
    fn promql_range_and_subquery_use_authoritative_edge_and_window_evidence() {
        use asap_types::pre_asap::{Column, DataType, QueryExpr, Schema, Source};
        use std::{rc::Rc, time::Duration};

        let source = Source::TimeSeries { metric: "m".into() };
        let scan = Rc::new(QueryExpr::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let range = Rc::new(QueryExpr::TimeRange {
            range: Duration::from_secs(300),
            child: scan,
        });
        let root = Rc::new(QueryExpr::PromqlSubquery {
            range: Duration::from_secs(300),
            resolution: Some(Duration::from_secs(60)),
            child: range,
        });
        let scope = scope(vec![coverage(source, vec![])]);
        let mut scan = with_promql(
            statistics(vec![edge(60_000, 960_000)], edge(60_000, 960_000)),
            vec![100],
            100,
            6,
        );
        scan.source_scan_bytes = 120_000;
        let mut range = with_promql(
            statistics(vec![edge(60_000, 960_000)], edge(600, 9_600)),
            vec![100],
            100,
            6,
        );
        range.promql.as_mut().unwrap().window_samples_per_series = Some(100);
        let mut subquery = with_promql(
            statistics(vec![edge(600, 9_600)], edge(100, 1_600)),
            vec![100],
            100,
            1,
        );
        subquery.promql.as_mut().unwrap().subquery_steps = Some(6);
        let provided = HashMap::from([
            ("query-2".into(), evidence(scan)),
            ("query-1".into(), evidence(range)),
            ("query-0".into(), evidence(subquery)),
        ]);

        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::PromqlRange,
                PhysicalOperator::PromqlSubquery,
            ]
        );
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).unwrap();
        assert_eq!(estimate.scan_bytes, 120_000);
        assert!(estimate.peak_memory_bytes >= 160_000);
    }

    #[test]
    fn nested_promql_subqueries_compose_internal_evaluation_steps() {
        use asap_types::pre_asap::{Column, DataType, QueryExpr, Schema, Source};
        use std::{rc::Rc, time::Duration};

        let source = Source::TimeSeries { metric: "m".into() };
        let scan = Rc::new(QueryExpr::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let range = Rc::new(QueryExpr::TimeRange {
            range: Duration::from_secs(60),
            child: scan,
        });
        let inner = Rc::new(QueryExpr::PromqlSubquery {
            range: Duration::from_secs(60),
            resolution: Some(Duration::from_secs(60)),
            child: range,
        });
        let root = Rc::new(QueryExpr::PromqlSubquery {
            range: Duration::from_secs(120),
            resolution: Some(Duration::from_secs(60)),
            child: inner,
        });
        let scope = scope(vec![coverage(source, vec![])]);
        let mut scan = with_promql(
            statistics(vec![edge(60, 960)], edge(60, 960)),
            vec![10],
            10,
            6,
        );
        scan.source_scan_bytes = 480;
        let mut range = with_promql(
            statistics(vec![edge(60, 960)], edge(60, 960)),
            vec![10],
            10,
            6,
        );
        range.promql.as_mut().unwrap().window_samples_per_series = Some(6);
        let mut inner = with_promql(
            statistics(vec![edge(60, 960)], edge(30, 480)),
            vec![10],
            10,
            3,
        );
        inner.promql.as_mut().unwrap().subquery_steps = Some(2);
        let mut outer = with_promql(
            statistics(vec![edge(30, 480)], edge(10, 160)),
            vec![10],
            10,
            1,
        );
        outer.promql.as_mut().unwrap().subquery_steps = Some(3);
        let provided = HashMap::from([
            ("query-3".into(), evidence(scan)),
            ("query-2".into(), evidence(range)),
            ("query-1".into(), evidence(inner)),
            ("query-0".into(), evidence(outer)),
        ]);
        assert!(lower_query_physical_dag(&root, &scope, &scripted(&provided)).is_ok());

        let mut overflow = provided;
        overflow
            .get_mut("query-0")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap()
            .evaluation_steps = u64::MAX;
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&overflow)),
            Err(AnalyticalCostError::Overflow)
        );
    }

    #[test]
    fn scalar_only_promql_accepts_empty_scope_but_scan_queries_do_not() {
        use asap_types::pre_asap::{Column, DataType, QueryExpr, Schema, Source};
        use std::rc::Rc;

        let empty_scope = scope(vec![]);
        let scalar = Rc::new(QueryExpr::promql_scalar(5.0));
        let scalar_statistics = with_promql(statistics(vec![], edge(10, 80)), vec![], 0, 10);
        let mut provided = HashMap::from([("query-0".into(), evidence(scalar_statistics))]);
        let dag = lower_query_physical_dag(&scalar, &empty_scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes[0].operator, PhysicalOperator::PromqlScalarLeaf);
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &empty_scope, &dag.evidence).is_ok());
        provided.get_mut("query-0").unwrap().statistics.output = edge(9, 72);
        assert!(matches!(
            lower_query_physical_dag(&scalar, &empty_scope, &scripted(&provided)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        let timestamp = Rc::new(QueryExpr::EvalTimestamp);
        let timestamp_statistics = with_promql(statistics(vec![], edge(10, 80)), vec![], 0, 10);
        let provided = HashMap::from([("query-0".into(), evidence(timestamp_statistics))]);
        let dag = lower_query_physical_dag(&timestamp, &empty_scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes[0].operator, PhysicalOperator::PromqlScalarLeaf);

        let vector = Rc::new(QueryExpr::PromqlVectorFromScalar(Rc::new(
            QueryExpr::promql_scalar(1.0),
        )));
        let scalar_statistics = with_promql(statistics(vec![], edge(10, 80)), vec![], 0, 10);
        let vector_statistics =
            with_promql(statistics(vec![edge(10, 80)], edge(10, 80)), vec![0], 1, 10);
        let mut provided = HashMap::from([
            ("query-1".into(), evidence(scalar_statistics)),
            ("query-0".into(), evidence(vector_statistics)),
        ]);
        let dag = lower_query_physical_dag(&vector, &empty_scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes[0].operator, PhysicalOperator::PromqlScalarLeaf);
        assert_eq!(
            dag.nodes[1].operator,
            PhysicalOperator::PromqlScalarToVector
        );

        provided
            .get_mut("query-0")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap()
            .output_series = 0;
        assert!(matches!(
            lower_query_physical_dag(&vector, &empty_scope, &scripted(&provided)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        let scalar_from_vector = Rc::new(QueryExpr::PromqlScalarFromVector(vector));
        let scalar_statistics = with_promql(statistics(vec![], edge(10, 80)), vec![], 0, 10);
        let vector_statistics =
            with_promql(statistics(vec![edge(10, 80)], edge(10, 80)), vec![0], 1, 10);
        let scalar_from_vector_statistics =
            with_promql(statistics(vec![edge(10, 80)], edge(10, 80)), vec![1], 0, 10);
        let mut provided = HashMap::from([
            ("query-2".into(), evidence(scalar_statistics)),
            ("query-1".into(), evidence(vector_statistics)),
            ("query-0".into(), evidence(scalar_from_vector_statistics)),
        ]);
        let dag = lower_query_physical_dag(&scalar_from_vector, &empty_scope, &scripted(&provided))
            .unwrap();
        assert_eq!(
            dag.nodes[2].operator,
            PhysicalOperator::PromqlVectorToScalar
        );
        provided
            .get_mut("query-0")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap()
            .output_series = 1;
        assert!(matches!(
            lower_query_physical_dag(&scalar_from_vector, &empty_scope, &scripted(&provided)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));
        let promql = provided
            .get_mut("query-0")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap();
        promql.output_series = 0;
        promql.input_series = vec![2];
        assert!(matches!(
            lower_query_physical_dag(&scalar_from_vector, &empty_scope, &scripted(&provided)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(_))
        ));

        let scan = Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        assert!(matches!(
            lower_query_physical_dag(&scan, &empty_scope, &scripted(&provided)),
            Err(AnalyticalCostError::ScanOutsideComparisonScope(_))
        ));
    }

    #[test]
    fn promql_info_enrichment_lowers_its_source_as_a_scoped_scan() {
        use asap_types::pre_asap::{
            Column, CompareOpKind, DataType, InfoMatcher, QueryExpr, Schema, Source,
        };
        use std::rc::Rc;

        let primary = Source::TimeSeries { metric: "m".into() };
        let info = Source::TimeSeries {
            metric: "target_info".into(),
        };
        let child = Rc::new(QueryExpr::Scan {
            source: primary.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let root = Rc::new(QueryExpr::PromqlInfoEnrich {
            selector: vec![InfoMatcher {
                label: "job".into(),
                op: CompareOpKind::Eq,
                value: "api".into(),
            }],
            child,
        });
        let scope = scope(vec![coverage(primary, vec![]), coverage(info, vec![])]);
        let mut left = with_promql(
            statistics(vec![edge(100, 1_600)], edge(100, 1_600)),
            vec![10],
            10,
            10,
        );
        left.source_scan_bytes = 800;
        let mut right = with_promql(
            statistics(vec![edge(20, 1_000)], edge(20, 1_000)),
            vec![2],
            2,
            10,
        );
        right.source_scan_bytes = 400;
        let mut enrich = with_promql(
            statistics(vec![edge(100, 1_600), edge(20, 1_000)], edge(100, 2_400)),
            vec![10, 2],
            10,
            10,
        );
        enrich.key_bytes = Some(16);
        enrich.hash_join_build_side = Some(HashJoinBuildSide::Right);
        enrich.promql.as_mut().unwrap().scalar_ops_per_row = Some(3);
        let provided = HashMap::from([
            ("query-1".into(), evidence(left)),
            ("query-0-info-scan".into(), evidence(right)),
            ("query-0".into(), evidence(enrich)),
        ]);

        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes[1].operator, PhysicalOperator::Scan);
        assert_eq!(dag.nodes[1].source_coverage, Some(scope.sources[1].clone()));
        assert_eq!(dag.nodes[2].children.len(), 2);
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).unwrap();
        assert_eq!(estimate.scan_bytes, 1_200);
        assert_eq!(estimate.cpu_ops, 400.0);

        let mut missing_matcher_work = provided.clone();
        missing_matcher_work
            .get_mut("query-0")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap()
            .scalar_ops_per_row = None;
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&missing_matcher_work)),
            Err(AnalyticalCostError::MissingOrZero(
                "info_matcher_ops_per_row"
            ))
        );

        let mut wrong_build_side = provided;
        wrong_build_side
            .get_mut("query-0")
            .unwrap()
            .statistics
            .hash_join_build_side = Some(HashJoinBuildSide::Left);
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&wrong_build_side)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "info enrichment must build its label index from the right side"
            ))
        );
    }

    #[test]
    fn promql_vector_scalar_binary_keeps_two_edges_without_label_state() {
        use asap_types::pre_asap::{
            ArithmeticOpKind, BinaryOpKind, Column, DataType, QueryExpr, Schema, Source,
        };
        use std::rc::Rc;

        let source = Source::TimeSeries { metric: "m".into() };
        let vector = Rc::new(QueryExpr::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let scalar = Rc::new(QueryExpr::promql_scalar(2.0));
        let root = Rc::new(QueryExpr::BinaryOp {
            op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            lhs: Rc::clone(&vector),
            rhs: scalar,
            vector_match: None,
        });
        let scope = scope(vec![coverage(source, vec![])]);
        let mut vector_statistics = with_promql(
            statistics(vec![edge(1_000, 16_000)], edge(1_000, 16_000)),
            vec![100],
            100,
            10,
        );
        vector_statistics.source_scan_bytes = 8_000;
        let scalar = with_promql(statistics(vec![], edge(10, 80)), vec![], 0, 10);
        let mut binary = with_promql(
            statistics(vec![edge(1_000, 16_000), edge(10, 80)], edge(1_000, 16_000)),
            vec![100, 0],
            100,
            10,
        );
        binary.promql.as_mut().unwrap().binary_operand_mode =
            Some(PromqlBinaryOperandMode::VectorScalar);
        let provided = HashMap::from([
            ("query-1".into(), evidence(vector_statistics)),
            ("query-2".into(), evidence(scalar)),
            ("query-0".into(), evidence(binary)),
        ]);

        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert_eq!(dag.nodes.last().unwrap().children.len(), 2);
        assert_eq!(
            dag.nodes.last().unwrap().operator,
            PhysicalOperator::PromqlVectorBinary
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());

        let eval_timestamp = Rc::new(QueryExpr::BinaryOp {
            op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            lhs: Rc::clone(&vector),
            rhs: Rc::new(QueryExpr::EvalTimestamp),
            vector_match: None,
        });
        assert!(lower_query_physical_dag(&eval_timestamp, &scope, &scripted(&provided)).is_ok());

        let sql_timestamp = Rc::new(QueryExpr::BinaryOp {
            op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            lhs: vector,
            rhs: Rc::new(QueryExpr::CurrentTimestamp),
            vector_match: None,
        });
        assert_eq!(
            lower_query_physical_dag(&sql_timestamp, &scope, &scripted(&provided)),
            Err(AnalyticalCostError::UnsupportedQueryOperator)
        );
    }

    #[test]
    fn promql_vector_binary_requires_explicit_label_match_state() {
        use asap_types::pre_asap::{
            ArithmeticOpKind, BinaryOpKind, Column, DataType, QueryExpr, Schema, Source,
            VectorMatch, VectorMatchKind,
        };
        use std::rc::Rc;

        let left_source = Source::TimeSeries {
            metric: "requests".into(),
        };
        let right_source = Source::TimeSeries {
            metric: "errors".into(),
        };
        let scan = |source: Source| {
            Rc::new(QueryExpr::Scan {
                source,
                predicates: vec![],
                schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
            })
        };
        let root = Rc::new(QueryExpr::BinaryOp {
            op: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
            lhs: scan(left_source.clone()),
            rhs: scan(right_source.clone()),
            vector_match: Some(VectorMatch {
                kind: VectorMatchKind::On,
                labels: vec!["service".into()],
                grouping: None,
            }),
        });
        let scope = scope(vec![
            coverage(left_source, vec![]),
            coverage(right_source, vec![]),
        ]);
        let mut left = with_promql(
            statistics(vec![edge(1_000, 16_000)], edge(1_000, 16_000)),
            vec![100],
            100,
            10,
        );
        left.source_scan_bytes = 8_000;
        let mut right = with_promql(
            statistics(vec![edge(500, 8_000)], edge(500, 8_000)),
            vec![50],
            50,
            10,
        );
        right.source_scan_bytes = 4_000;
        let mut binary = with_promql(
            statistics(
                vec![edge(1_000, 16_000), edge(500, 8_000)],
                edge(400, 6_400),
            ),
            vec![100, 50],
            40,
            10,
        );
        binary.key_bytes = Some(32);
        binary.hash_join_build_side = Some(HashJoinBuildSide::Right);
        binary.promql.as_mut().unwrap().binary_operand_mode =
            Some(PromqlBinaryOperandMode::VectorVector);
        let provided = HashMap::from([
            ("query-1".into(), evidence(left)),
            ("query-2".into(), evidence(right)),
            ("query-0".into(), evidence(binary)),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        let estimate = estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).unwrap();
        assert_eq!(estimate.cpu_ops, 3_400.0);
        assert_eq!(estimate.scan_bytes, 12_000);

        let mut empty = provided.clone();
        for id in ["query-1", "query-2"] {
            let evidence = empty.get_mut(id).unwrap();
            evidence.statistics.inputs[0] = edge(0, 0);
            evidence.statistics.output = edge(0, 0);
            let promql = evidence.statistics.promql.as_mut().unwrap();
            promql.input_series[0] = 0;
            promql.output_series = 0;
            evidence.output_buffer_bytes = 0;
        }
        let evidence = empty.get_mut("query-0").unwrap();
        evidence.statistics.inputs = vec![edge(0, 0), edge(0, 0)];
        evidence.statistics.output = edge(0, 0);
        let promql = evidence.statistics.promql.as_mut().unwrap();
        promql.input_series = vec![0, 0];
        promql.output_series = 0;
        evidence.output_buffer_bytes = 0;
        let empty_dag = lower_query_physical_dag(&root, &scope, &scripted(&empty)).unwrap();
        assert!(estimate_physical_dag(
            &empty_dag.nodes,
            &empty_dag.root,
            &scope,
            &empty_dag.evidence
        )
        .is_ok());

        let mut missing = provided;
        missing
            .get_mut("query-0")
            .unwrap()
            .statistics
            .hash_join_build_side = None;
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&missing)),
            Err(AnalyticalCostError::MissingOrStale(
                "vector_binary_label_match_statistics"
            ))
        );
    }

    #[test]
    fn promql_relabel_sample_and_per_series_operators_are_costed() {
        use asap_types::pre_asap::{
            AggIntent, Column, DataType, GroupKeys, QueryExpr, Reduction, SampleKind, Schema,
            Source,
        };
        use std::rc::Rc;

        let source = Source::TimeSeries { metric: "m".into() };
        let scan = Rc::new(QueryExpr::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let relabel = Rc::new(QueryExpr::PromqlRelabel {
            dst: "service".into(),
            value: Rc::new(QueryExpr::FunctionCall {
                name: "label_replace".into(),
                args: vec![],
            }),
            child: scan,
        });
        let sample = Rc::new(QueryExpr::PromqlSeriesSample {
            by: GroupKeys::none(),
            kind: SampleKind::LimitK(10),
            child: relabel,
        });
        let root = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: sample,
        });
        let scope = scope(vec![coverage(source, vec![])]);
        let mut scan = with_promql(
            statistics(vec![edge(1_000, 16_000)], edge(1_000, 16_000)),
            vec![100],
            100,
            10,
        );
        scan.source_scan_bytes = 8_000;
        let mut relabel = with_promql(
            statistics(vec![edge(1_000, 16_000)], edge(1_000, 20_000)),
            vec![100],
            100,
            10,
        );
        relabel.promql.as_mut().unwrap().scalar_ops_per_row = Some(8);
        let mut sample = with_promql(
            statistics(vec![edge(1_000, 20_000)], edge(100, 2_000)),
            vec![100],
            10,
            10,
        );
        sample.group_count = Some(1);
        sample.key_bytes = Some(24);
        sample.k = Some(10);
        let mut aggregate = with_promql(
            statistics(vec![edge(100, 2_000)], edge(10, 200)),
            vec![10],
            10,
            10,
        );
        aggregate.aggregate_value_bytes = Some(16);
        aggregate.promql.as_mut().unwrap().scalar_ops_per_row = Some(2);
        let provided = HashMap::from([
            ("query-3".into(), evidence(scan)),
            ("query-2".into(), evidence(relabel)),
            ("query-1".into(), evidence(sample)),
            ("query-0".into(), evidence(aggregate)),
        ]);

        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert_eq!(
            dag.nodes
                .iter()
                .map(|node| node.operator)
                .collect::<Vec<_>>(),
            vec![
                PhysicalOperator::Scan,
                PhysicalOperator::PromqlRelabel,
                PhysicalOperator::PromqlSeriesSample,
                PhysicalOperator::PromqlPerSeries,
            ]
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());

        let mut shrinking = provided;
        shrinking
            .get_mut("query-2")
            .unwrap()
            .statistics
            .promql
            .as_mut()
            .unwrap()
            .output_series = 99;
        assert_eq!(
            lower_query_physical_dag(&root, &scope, &scripted(&shrinking)),
            Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "relabel changes row or series cardinality"
            ))
        );
    }

    #[test]
    fn promql_over_time_and_native_histogram_intents_use_per_series_work() {
        use asap_types::pre_asap::AggIntent;
        use asap_types::types::AccuracyTarget;

        let intents = vec![
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            },
            AggIntent::Sum { col: None },
            AggIntent::Min { col: None },
            AggIntent::Max { col: None },
            AggIntent::Avg { col: None },
            AggIntent::StdDev {
                col: None,
                population: true,
            },
            AggIntent::Variance {
                col: None,
                population: true,
            },
            AggIntent::HistogramCount,
            AggIntent::HistogramSum,
            AggIntent::HistogramAvg,
            AggIntent::HistogramStdDev,
            AggIntent::HistogramStdVar,
            AggIntent::HistogramFraction {
                lower: 0.0,
                upper: 1.0,
            },
        ];
        for intent in intents {
            assert_eq!(
                aggregate_operator(&asap_types::pre_asap::Reduction::PerEntity, &[intent]),
                Some(PhysicalOperator::PromqlPerSeries)
            );
        }
    }

    #[test]
    fn promql_presence_can_synthesize_one_row_from_an_empty_scan() {
        use asap_types::pre_asap::{
            AggIntent, Column, DataType, QueryExpr, Reduction, Schema, Source,
        };
        use std::rc::Rc;

        let source = Source::TimeSeries {
            metric: "missing".into(),
        };
        let scan = Rc::new(QueryExpr::Scan {
            source: source.clone(),
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let root = Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Absent],
            output_names: vec![],
            having: None,
            child: scan,
        });
        let scope = scope(vec![coverage(source, vec![])]);
        let scan = with_promql(statistics(vec![edge(0, 0)], edge(0, 0)), vec![0], 0, 1);
        let mut presence = with_promql(statistics(vec![edge(0, 0)], edge(1, 8)), vec![0], 1, 1);
        presence.promql.as_mut().unwrap().scalar_ops_per_row = Some(1);
        let provided = HashMap::from([
            ("query-1".into(), evidence(scan)),
            ("query-0".into(), evidence(presence)),
        ]);
        let dag = lower_query_physical_dag(&root, &scope, &scripted(&provided)).unwrap();
        assert_eq!(
            dag.nodes.last().unwrap().operator,
            PhysicalOperator::PromqlPresence
        );
        assert!(estimate_physical_dag(&dag.nodes, &dag.root, &scope, &dag.evidence).is_ok());
    }
}
