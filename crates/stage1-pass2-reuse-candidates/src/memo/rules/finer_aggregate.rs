//! Read a SQL aggregate from a finer aggregate's result
//! ([`match_finer`](crate::memo::rules::finer_aggregate::match_finer)): filter the finer
//! rows, then merge them into the query's groups with each measure's rollup.

use crate::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Rule 5, finer aggregate: a candidate of a coarse group that filters and rolls up a finer group.
pub(crate) fn add_finer_aggregate_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 5 over groups")
}
