//! The metric of a readout's error: what a `SummaryEstimate` of a sketch
//! state may be off in.

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroup, Memo};
use asap_types::summary_type::{parameter_list, SketchStatistic};

use crate::guarantee::guarantee_before_failure_probability_check;

/// The metric of the error reading `statistic` from a state of
/// `summary_type` has: its first listed θ's guarantee's, whatever that
/// guarantee's failure probability (HLL's distinct count reads a relative
/// error although its failure probability is unknown). `None` for an exact
/// state and for a summary type with no listed θ or no guarantee for the
/// statistic (UnivMon's distinct count).
pub fn readout_metric(
    summary_type: &FieldDataType,
    statistic: &SketchStatistic,
) -> Option<AccuracyMetric> {
    let FieldDataType::Sketch(algorithm) = summary_type else {
        return None;
    };
    let parameters = parameter_list(*algorithm).first()?;
    Some(
        guarantee_before_failure_probability_check(summary_type, parameters, statistic, 1.0)?
            .spec
            .accuracy_metric,
    )
}

/// The metric of `candidate`'s error when it is a readout
/// (`SummaryEstimate`) of a sketch state ([`readout_metric`]); `None` for
/// any other candidate.
pub(crate) fn sketch_readout_metric(
    memo: &Memo,
    candidate: &CandidateForMemoGroup,
) -> Option<AccuracyMetric> {
    let (summary_type, statistic) = sketch_readout(memo, candidate)?;
    readout_metric(&summary_type, statistic)
}

/// Per group, by group id, per candidate, by index in the group: its
/// [`sketch_readout_metric`].
pub fn readout_metric_of_candidate(memo: &Memo) -> Vec<Vec<Option<AccuracyMetric>>> {
    memo.groups()
        .iter()
        .map(|group| {
            group
                .candidates
                .iter()
                .map(|candidate| sketch_readout_metric(memo, candidate))
                .collect()
        })
        .collect()
}

/// The summary type of the sketch state `candidate` reads and the statistic
/// it reads from it, when it is a readout (`SummaryEstimate`) of a sketch
/// state (`GroupProperties::sketch_algorithm_of_state`).
pub(crate) fn sketch_readout<'a>(
    memo: &Memo,
    candidate: &'a CandidateForMemoGroup,
) -> Option<(FieldDataType, &'a SketchStatistic)> {
    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = &candidate.expression.operator
    else {
        return None;
    };
    let algorithm = memo.groups()[summary_input.0 as usize]
        .group_properties
        .sketch_algorithm_of_state()?;
    Some((FieldDataType::Sketch(algorithm), query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::summary_type::{ExactKind, SketchAlgorithm};

    /// A readout's metric is its algorithm's: KLL quantiles read a rank
    /// error, DDSketch quantiles and HLL distinct counts a relative one
    /// (HLL's although its failure probability is unknown); an exact state
    /// reads exactly.
    #[test]
    fn a_readouts_metric_is_its_algorithms() {
        let quantile = SketchStatistic::Quantile { q: 0.9 };
        let sketch = |algorithm| FieldDataType::Sketch(algorithm);
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::Kll), &quantile),
            Some(AccuracyMetric::Rank)
        );
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::DDSketch), &quantile),
            Some(AccuracyMetric::Relative)
        );
        assert_eq!(
            readout_metric(&sketch(SketchAlgorithm::Hll), &SketchStatistic::Cardinality),
            Some(AccuracyMetric::Relative)
        );
        assert_eq!(
            readout_metric(&FieldDataType::exact(ExactKind::Sum), &quantile),
            None
        );
    }
}
