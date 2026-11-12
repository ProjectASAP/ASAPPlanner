//! Step 3 (concepts doc, section 3, "Reach, caps and readout requirements";
//! section 4, "Readout requirements"): writes each readout's required
//! accuracy, from the levels `asap_accuracy::readout_levels` gives it, on
//! the readout and its copies.

use std::collections::{BTreeMap, BTreeSet};

use crate::Pass2RuleError;
use asap_accuracy::{readout_levels, ErrorMetricsOfMemo};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId, RequiredAccuracy,
};

/// What step 3 leaves for the later steps beside the requirements it
/// writes.
pub(crate) struct ReadoutRequirementsOutcome {
    /// The readouts no query gives a level while one reaching them has a
    /// failure probability without a per-value one, each with that query:
    /// Pass 2 prunes them (`RootPruneReason::ValuesPerEvaluationUnknown`).
    pub(crate) readouts_without_per_value_failure_probability:
        BTreeMap<CandidateForMemoGroupId, usize>,
    /// Per such query, the readout groups it reaches without a per-value
    /// failure probability, which it may read only exactly (another
    /// query's level says nothing of its own δ).
    pub(crate) exact_only_readout_groups_of_query: BTreeMap<usize, BTreeSet<MemoGroupId>>,
}

/// Gives every sketch readout (`SummaryEstimate` over a sketch state) its
/// required accuracy, one readout candidate per requirement level
/// ([`readout_levels`]): Pass 1's candidate takes the first level and a
/// copy is added per further level. A readout without a level keeps no
/// requirement. `order` is the memo's topological order.
pub(crate) fn set_readout_requirements(
    memo: &mut Memo,
    order: &[MemoGroupId],
) -> Result<ReadoutRequirementsOutcome, Pass2RuleError> {
    let mut outcome = ReadoutRequirementsOutcome {
        readouts_without_per_value_failure_probability: BTreeMap::new(),
        exact_only_readout_groups_of_query: BTreeMap::new(),
    };
    let error_metrics = ErrorMetricsOfMemo::of(memo, order);
    for (readout, required) in readout_levels(memo, &error_metrics, order) {
        for &query in &required.queries_without_per_value_failure_probability {
            outcome
                .exact_only_readout_groups_of_query
                .entry(query)
                .or_default()
                .insert(readout.group);
        }
        match required
            .queries_without_per_value_failure_probability
            .first()
        {
            Some(&query) if required.levels.is_empty() => {
                outcome
                    .readouts_without_per_value_failure_probability
                    .insert(readout, query);
            }
            _ => write_levels(memo, readout, &required.levels)?,
        }
    }
    Ok(outcome)
}

/// Writes `levels` on `readout`: the first on it, each further one on a
/// copy of it added to its group with that level. No level leaves it
/// without a requirement.
fn write_levels(
    memo: &mut Memo,
    readout: CandidateForMemoGroupId,
    levels: &[RequiredAccuracy],
) -> Result<(), Pass2RuleError> {
    let requirement = |level: RequiredAccuracy| {
        vec![LogicalRequirements {
            required_accuracy: Some(level),
        }]
    };
    let Some((&first, further)) = levels.split_first() else {
        return Ok(());
    };
    let unset = memo.candidate(readout).clone();
    memo.set_candidate_input_requirements(readout, requirement(first));
    for &level in further {
        let mut copy = unset.clone();
        copy.input_requirements = requirement(level);
        memo.add_candidate(readout.group, copy)
            .map_err(Pass2RuleError::Candidate)?;
    }
    Ok(())
}
