//! Expose private value projections and filters below summary builds as canonical inputs.
//! This preserves their full expressions and schema, including opaque series
//! identity, rather than assuming that a projection is an identity operation.

use crate::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Rule 3, summary inputs: a value projection or filter below a summary build as its own input group.
pub(crate) fn factor_summary_inputs(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 3 over groups")
}
