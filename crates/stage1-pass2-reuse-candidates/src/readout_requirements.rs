//! Step 3 (concepts doc, section 3, "Reach, caps and readout requirements";
//! section 4, "Readout requirements"): writes each readout's required
//! accuracy, from the levels `asap_accuracy::readout_levels` gives it, on
//! the readout and its copies.

use std::collections::{BTreeMap, BTreeSet};

use crate::Pass2RuleError;
use asap_accuracy::readout_levels;
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalRequirements, Memo, MemoGroupId, RequiredAccuracy,
};

/// Gives every sketch readout (`SummaryEstimate` over a sketch state) its
/// required accuracy, one readout candidate per requirement level
/// ([`readout_levels`]): Pass 1's candidate takes the first level and a
/// copy is added per further level, each written with
/// `Memo::set_candidate_input_requirements`. A readout without a level
/// keeps no requirement. `order` is the memo's topological order.
///
/// Returns, first, the readouts that no query gives a level while one
/// reaching them has a failure probability without a per-value one, each
/// with that query: Pass 2 prunes them
/// (`RootPruneReason::ValuesPerEvaluationUnknown`); second, per such query,
/// the readout groups it reaches without a per-value failure probability,
/// which it may read only exactly (another query's level says nothing of
/// its own δ).
#[allow(clippy::type_complexity)]
pub(crate) fn set_readout_requirements(
    memo: &mut Memo,
    order: &[MemoGroupId],
) -> Result<
    (
        BTreeMap<CandidateForMemoGroupId, usize>,
        BTreeMap<usize, BTreeSet<MemoGroupId>>,
    ),
    Pass2RuleError,
> {
    let mut readouts_without_per_value_failure_probability = BTreeMap::new();
    let mut read_only_exactly: BTreeMap<usize, BTreeSet<MemoGroupId>> = BTreeMap::new();
    for (readout, (levels, queries_without_per_value_failure_probability)) in
        readout_levels(memo, order)
    {
        for &query in &queries_without_per_value_failure_probability {
            read_only_exactly
                .entry(query)
                .or_default()
                .insert(readout.group);
        }
        match queries_without_per_value_failure_probability.first() {
            Some(&query) if levels.is_empty() => {
                readouts_without_per_value_failure_probability.insert(readout, query);
            }
            _ => write_levels(memo, readout, &levels)?,
        }
    }
    Ok((
        readouts_without_per_value_failure_probability,
        read_only_exactly,
    ))
}
/// Writes `levels` on `readout`: the first on it, each further one on a
/// copy of it added to its group. No level leaves it without a
/// requirement.
fn write_levels(
    memo: &mut Memo,
    readout: CandidateForMemoGroupId,
    levels: &[RequiredAccuracy],
) -> Result<(), Pass2RuleError> {
    let unset = memo.candidate(readout).clone();
    for (n, &level) in levels.iter().enumerate() {
        let id = if n == 0 {
            readout
        } else {
            memo.add_candidate(readout.group, unset.clone())
                .map_err(Pass2RuleError::Candidate)?
        };
        memo.set_candidate_input_requirements(
            id,
            vec![LogicalRequirements {
                required_accuracy: Some(level),
            }],
        );
    }
    Ok(())
}
