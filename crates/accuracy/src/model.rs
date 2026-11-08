//! The deployment's measured accuracy, and α^log of a logical edge.

use asap_types::ir::properties::AccuracyGuarantee;
use asap_types::ir::schema::{FieldDataType, SketchParams, SketchStatistic};
use asap_types::stages::{CandidateForMemoGroupId, Memo};

/// The deployment's measured accuracy, per summary configuration. A
/// producer's guarantee for a readout is its measurement when there is one,
/// else [`theoretical_guarantee`](crate::estimators::theoretical_guarantee);
/// the source is decided before Pass 2 builds the logical edges (concepts
/// doc, section 2, "Where the guarantee comes from").
#[derive(Debug, Clone, Default)]
pub struct AccuracyEvidence {}

impl AccuracyEvidence {
    /// The measured guarantee of reading `statistic` from a state of
    /// `summary_type` with parameters `parameters`; `None` when the
    /// deployment has no measurement for it. No measurement source exists
    /// yet, so every configuration has none.
    pub fn measured_guarantee(
        &self,
        summary_type: &FieldDataType,
        parameters: &SketchParams,
        statistic: &SketchStatistic,
    ) -> Option<AccuracyGuarantee> {
        let _ = (summary_type, parameters, statistic);
        None
    }
}

/// $\alpha^{\log}$ of one edge (concepts doc, section 2): for a readout's
/// input, the producer's guarantee for the readout's statistic at the
/// required failure probability is within the required error, and a
/// top-k's heap holds k; for any other input, the metric rule (section 4).
pub fn producer_satisfies_logical_requirements(
    memo: &Memo,
    producer: CandidateForMemoGroupId,
    consumer: CandidateForMemoGroupId,
    input_index_of_consumer: usize,
    evidence: &AccuracyEvidence,
) -> bool {
    let _ = (memo, producer, consumer, input_index_of_consumer, evidence);
    todo!("#1207 PR 4: the readout rule against RequiredAccuracy, else the metric rule")
}
