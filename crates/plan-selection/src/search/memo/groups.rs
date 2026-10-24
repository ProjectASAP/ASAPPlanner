//! Compatibility entry points while selection callers migrate to logical memo APIs.
//! Candidate population and its tests live in logical ASAP optimization; this
//! module owns no generation rules, cost model, or solver state.

use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;

use super::Memo;
pub(crate) use asap_logical_asap_optimization::memo::candidates::NodeKeys;

pub(crate) fn memo_of<Id: Clone>(inventory: &PerComputationCandidates<Id>) -> Memo {
    populate_pass1(LogicalMemo::from_roots(&inventory.roots), inventory)
}

/// Internal callers generate and prune this inventory from the retained roots.
pub(crate) fn populate_pass1<Id: Clone>(
    initial: LogicalMemo,
    inventory: &PerComputationCandidates<Id>,
) -> Memo {
    try_populate_pass1(initial, inventory)
        .expect("internally generated Pass 1 inventory must match its logical memo")
}
