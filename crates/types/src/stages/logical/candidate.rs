//! Candidate scope: each group's candidates $v = (e_v, p^{\log}_v)$
//! (concepts doc `memo-groups-properties-requirements.md`, section 3;
//! #1203).

use crate::ir::operator::asap::summary_type_of_operator;
use crate::ir::schema::FieldDataType;
use crate::ir::Operator;
use crate::ir::SchemaDerivationError;
use crate::stages::{LogicalRequirements, MemoGroupId};
use crate::summary_type::{parameter_list, SketchParams};

/// One candidate: position `index_in_group` in group `group`'s candidates
/// ([`MemoGroup::candidates`](crate::stages::MemoGroup::candidates)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index_in_group: usize,
}

/// $v = (e_v, p^{\log}_v)$: one candidate of a memo group.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// $e_v$.
    pub expression: CandidateExpression,
    /// $p^{\log}_v$.
    pub logical_properties: LogicalProperties,
    /// $r^{\log}_{c,i}$, one per input group: per child of
    /// `expression.operator`, in [`Operator::children`] order. Changed only
    /// through [`Memo::set_candidate_input_requirements`](crate::stages::Memo::set_candidate_input_requirements).
    pub input_requirements: Vec<LogicalRequirements>,
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is; empty otherwise.
    pub fused_groups: Vec<MemoGroupId>,
    /// Which rule added it. Not in the formulation: kept for
    /// `stage_pipeline --text` and the viewer (approved 2026-11-05).
    pub origin: OriginOfCandidateForMemoGroup,
}

/// Which rule added a candidate. Nothing in planning reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary type.
    Pass1,
    /// Canonical value preprocessing feeding a summary build.
    SummaryInput,
    /// A tumbling-window chain, or a merge reading one, of a summary state group.
    TumblingWindows,
    /// A coarse group regrouping a finer group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (scan widening).
    WidenedScans,
}

/// $e_v = \operatorname{op}_v(G_{v,1}, \dots, G_{v,n})$: one operator, with
/// its arguments, whose children are groups. The input groups $G_{v,i}$ are
/// [`Operator::children`], in that order.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateExpression {
    pub operator: Operator<MemoGroupId>,
}

/// A candidate [`Memo::add_candidate`](crate::stages::Memo::add_candidate) rejects; the memo is unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CandidateError {
    /// The candidate's expression fails the node rules (schema derivation,
    /// input validation, merge check): a bug in the rule that built it.
    #[error("the candidate's expression breaks the node rules: {0}")]
    Schema(SchemaDerivationError),
    /// Its output schema or output kind differs from the group's: it does
    /// not compute the group's result (a bug in the rule that built it).
    #[error("the candidate's output differs from group {group:?}'s")]
    OutputDiffersFromGroup { group: MemoGroupId },
}

/// $p^{\log}_v = \boldsymbol\theta_v$. The summary type is the operator's
/// argument, not a property, and a tumbling-window chain's width is the
/// chain operator's argument. [`Self::new`] rejects parameters of another
/// algorithm, not in the algorithm's list, missing on a sketch build (a chain
/// included) or present on an exact operator.
#[derive(Debug, Clone)]
pub struct LogicalProperties {
    /// $\boldsymbol\theta_v$, from the algorithm's predefined list; `None` for
    /// an exact operator.
    pub(super) parameters: Option<SketchParams>,
}

impl LogicalProperties {
    pub fn new(
        expression: &CandidateExpression,
        parameters: Option<SketchParams>,
    ) -> Result<Self, LogicalPropertiesError> {
        let algorithm = match summary_type_of_operator(&expression.operator) {
            Some(FieldDataType::Sketch(algorithm)) => Some(*algorithm),
            _ => None,
        };
        match (algorithm, &parameters) {
            (Some(_), None) => return Err(LogicalPropertiesError::SketchBuildWithoutParameters),
            (None, Some(_)) => return Err(LogicalPropertiesError::ParametersOnExactOperator),
            (Some(algorithm), Some(parameters)) => {
                if !parameter_list(algorithm).contains(parameters) {
                    return Err(LogicalPropertiesError::ParametersNotInList);
                }
            }
            (None, None) => {}
        }
        Ok(Self { parameters })
    }

    /// $\boldsymbol\theta_v$.
    pub fn parameters(&self) -> Option<&SketchParams> {
        self.parameters.as_ref()
    }
}

/// Logical properties that do not fit their operator: bad input to
/// [`LogicalProperties::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LogicalPropertiesError {
    #[error("an exact operator has no summary parameters")]
    ParametersOnExactOperator,
    #[error("a sketch build needs parameters from its algorithm's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their algorithm's predefined list")]
    ParametersNotInList,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ASAPOp;
    use crate::summary_type::SketchAlgorithm;

    use crate::ir::operator::arguments::Reduction;

    use crate::ir::scalar::expr_ir::ColumnRef;

    use crate::ir::schema::FieldDataType;
    use crate::summary_type::{ExactKind, HydraParams, SummaryUpdate};

    /// `SummaryAgg` over group 0 building a state of `summary_type`.
    fn summary_agg(summary_type: FieldDataType) -> CandidateExpression {
        CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryAgg {
                child: MemoGroupId(0),
                summary_type,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            }),
        }
    }

    const KLL_269: SketchParams = SketchParams::Kll { k: 269 };

    const CMS: SketchParams = SketchParams::Cms {
        width: 272,
        depth: 5,
    };

    const HYDRA_CMS: SketchParams = SketchParams::Hydra(HydraParams::HydraCms {
        width: 272,
        depth: 5,
        shared_rows: 3,
        shared_columns: 1_024,
    });

    /// `LogicalProperties::new` over every operator kind and parameters: a
    /// sketch build (a chain included) needs parameters of its algorithm from
    /// its list; parameters are rejected on an operator that builds no
    /// sketch.
    #[test]
    fn logical_properties_new_over_every_operator_and_parameters() {
        use LogicalPropertiesError as Invalid;
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let kll_chain = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: MemoGroupId(0),
                summary_type: kll.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms: 5_000,
            }),
        };
        let readout = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: MemoGroupId(0),
                query: crate::summary_type::SketchStatistic::Quantile { q: 0.5 },
            }),
        };
        // k = 270 is KLL's structure but not in its list.
        let parameters = [
            None,
            Some(KLL_269),
            Some(CMS),
            Some(HYDRA_CMS),
            Some(SketchParams::Kll { k: 270 }),
        ];
        let no_parameters = Err(Invalid::ParametersOnExactOperator);
        let needs_parameters = Err(Invalid::SketchBuildWithoutParameters);
        let not_in_list = Err(Invalid::ParametersNotInList);
        let exact = [
            Ok(()),
            no_parameters,
            no_parameters,
            no_parameters,
            no_parameters,
        ];
        // Another algorithm's parameters are not in KLL's list either.
        let kll_build = [
            needs_parameters,
            Ok(()),
            not_in_list,
            not_in_list,
            not_in_list,
        ];
        // Per operator: the result for each of `parameters`.
        let table = [
            (summary_agg(kll), kll_build),
            // No Hydra θ is listed yet, so no Hydra build is valid.
            (
                summary_agg(FieldDataType::Sketch(SketchAlgorithm::HydraCms)),
                [
                    needs_parameters,
                    not_in_list,
                    not_in_list,
                    not_in_list,
                    not_in_list,
                ],
            ),
            (summary_agg(FieldDataType::exact(ExactKind::Sum)), exact),
            (kll_chain, kll_build),
            (readout, exact),
        ];
        for (expression, by_parameters) in table {
            for (parameters, expected) in parameters.iter().zip(by_parameters) {
                let properties = LogicalProperties::new(&expression, parameters.clone());
                assert_eq!(
                    properties.as_ref().map(|_| ()).map_err(|e| *e),
                    expected,
                    "{:?} {parameters:?}",
                    expression.operator.kind_name()
                );
                if let Ok(properties) = properties {
                    assert_eq!(properties.parameters(), parameters.as_ref());
                }
            }
        }
    }

    /// `summary_type_of_operator` reads the summary type of a `SummaryAgg`
    /// and of a chain, and of no other operator.
    #[test]
    fn summary_type_of_operator_reads_the_builds_summary_type() {
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: MemoGroupId(0),
            summary_type: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 5_000,
        });
        let merge = Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![MemoGroupId(0)],
            group_by: None,
        });
        assert_eq!(
            summary_type_of_operator(&summary_agg(kll.clone()).operator),
            Some(&kll)
        );
        assert_eq!(summary_type_of_operator(&chain), Some(&kll));
        assert_eq!(summary_type_of_operator(&merge), None);
    }
}
