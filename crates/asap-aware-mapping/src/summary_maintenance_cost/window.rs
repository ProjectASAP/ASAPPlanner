use super::*;

/// One per-state window choice within a complete Planner candidate.
#[derive(Debug, Clone)]
pub struct StreamingWindowFrameworkAssignment {
    pub summary: Rc<SummaryNode>,
    /// `None` explicitly means that this state is not window-organized.
    pub framework: Option<SummaryWindowFramework>,
}

/// Cost evidence for one complete abstract window-framework assignment across
/// a summary DAG in Planner search.
///
/// The provider derives this evidence from a concrete downstream
/// implementation under the current data workload. The stable identity is
/// provenance for the chosen implementation, while deployment placement and
/// runtime configuration remain downstream concerns.
#[derive(Debug, Clone)]
pub struct StreamingWindowFrameworkCandidate {
    /// Stable identity of the complete provider implementation whose evidence
    /// is bound to this planner-visible framework assignment.
    pub physical_plan_id: String,
    /// Exactly one assignment for every summary deployment in the DAG.
    pub assignments: Vec<StreamingWindowFrameworkAssignment>,
    /// Registered end-to-end accuracy composition for this complete window
    /// assignment. EH combinations must use one of the specialized proofs;
    /// unknown combinations fail closed.
    pub accuracy: StreamingWindowAccuracyEvidence,
    pub node_evidence: StreamingNodeEvidence,
}

/// Cardinality normalization used by the PromSketch EH bounds. The paper's
/// sub-window error is stated relative to the suffix beginning at the query's
/// left endpoint, so a query-relative bound needs `suffix_rows/query_rows`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExponentialHistogramQueryRange {
    MostRecentWindow,
    SubWindow { suffix_rows: u64, query_rows: u64 },
}

/// Registered accuracy compositions for Exponential Histogram realizations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExponentialHistogramAccuracyEvidence {
    /// PromSketch EHKLL normalized rank error.
    KllRank {
        eh_epsilon: f64,
        kll_epsilon: f64,
        failure_probability: f64,
        range: ExponentialHistogramQueryRange,
    },
    /// PromSketch EHUniv/GSum relative error.
    UniversalGsum {
        epsilon: f64,
        failure_probability: f64,
        range: ExponentialHistogramQueryRange,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamingWindowAccuracyEvidence {
    /// The window implementation preserves exact query-time coverage and adds
    /// no error. Used for exact tumbling/sliding realizations.
    Exact,
    ExponentialHistogram(ExponentialHistogramAccuracyEvidence),
}

impl ExponentialHistogramQueryRange {
    fn suffix_to_query_ratio(self) -> Option<f64> {
        match self {
            Self::MostRecentWindow => Some(1.0),
            Self::SubWindow {
                suffix_rows,
                query_rows,
            } if query_rows > 0 && suffix_rows >= query_rows => {
                Some(suffix_rows as f64 / query_rows as f64)
            }
            Self::SubWindow { .. } => None,
        }
    }
}

impl StreamingWindowAccuracyEvidence {
    fn matches_assignments(&self, assignments: &[StreamingWindowFrameworkAssignment]) -> bool {
        let eh_summaries: Vec<_> = assignments
            .iter()
            .filter(|assignment| {
                assignment.framework == Some(SummaryWindowFramework::ExponentialHistogram)
            })
            .collect();
        match self {
            Self::Exact => eh_summaries.is_empty(),
            Self::ExponentialHistogram(ExponentialHistogramAccuracyEvidence::KllRank {
                ..
            }) => {
                !eh_summaries.is_empty()
                    && eh_summaries.iter().all(|assignment| {
                        matches!(
                            &assignment.summary.expr,
                            SummaryExpr::SummaryAgg {
                                family: SummaryFamilyType::Sketch(kind, _),
                                ..
                            } if kind.algorithm() == &SketchAlgorithm::Kll
                        )
                    })
            }
            Self::ExponentialHistogram(ExponentialHistogramAccuracyEvidence::UniversalGsum {
                ..
            }) => {
                !eh_summaries.is_empty()
                    && eh_summaries.iter().all(|assignment| {
                        matches!(
                            &assignment.summary.expr,
                            SummaryExpr::SummaryAgg {
                                family: SummaryFamilyType::ExactAggregate(
                                    ExactKind::Count | ExactKind::Sum,
                                    _
                                ),
                                ..
                            }
                        )
                    })
            }
        }
    }

    /// Compose the two EH combinations proved by PromSketch
    /// (doi:10.14778/3742728.3742732). Unknown EH combinations deliberately
    /// have no catch-all arm.
    fn guarantee(&self, uses_exponential_histogram: bool) -> Option<ResultGuarantee> {
        match self {
            Self::Exact if !uses_exponential_histogram => {
                Some(ResultGuarantee::exact("exact window coverage"))
            }
            Self::Exact => None,
            Self::ExponentialHistogram(evidence) if uses_exponential_histogram => {
                let (metric, bound, failure_probability, rule) = match *evidence {
                    ExponentialHistogramAccuracyEvidence::KllRank {
                        eh_epsilon,
                        kll_epsilon,
                        failure_probability,
                        range,
                    } => (
                        ErrorMetric::Rank,
                        2.0 * eh_epsilon * range.suffix_to_query_ratio()? + kll_epsilon,
                        failure_probability,
                        "promsketch_eh_kll_rank",
                    ),
                    ExponentialHistogramAccuracyEvidence::UniversalGsum {
                        epsilon,
                        failure_probability,
                        range,
                    } => (
                        ErrorMetric::RelativeValue,
                        epsilon * range.suffix_to_query_ratio()?,
                        failure_probability,
                        "promsketch_eh_universal_gsum",
                    ),
                };
                if !bound.is_finite()
                    || bound < 0.0
                    || !failure_probability.is_finite()
                    || !(0.0..=1.0).contains(&failure_probability)
                {
                    return None;
                }
                Some(ResultGuarantee {
                    metric,
                    bound: BoundExpr::Constant { value: bound },
                    failure_probability: ProbabilityExpr::Constant {
                        value: failure_probability,
                    },
                    provenance: vec![GuaranteeSource::RuntimeObservation {
                        source: rule.into(),
                        detail: serde_json::json!({
                            "reference": "doi:10.14778/3742728.3742732"
                        }),
                    }],
                })
            }
            Self::ExponentialHistogram(_) => None,
        }
    }
}
