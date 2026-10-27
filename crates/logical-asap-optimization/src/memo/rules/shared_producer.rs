//! The shared-producer rule: readers that need one computation at different
//! values (a top-k `k`, a sketch size, a window and step) share one producer
//! built at a need that covers them, and each reader derives its own result
//! from it as one more candidate; its own candidates stay, so plan selection
//! decides. Identical expressions are the trivial case, shared by memo group
//! identity, and add no candidates here.
use crate::memo::candidates::CandidateMemo;
use crate::memo::check_generation_deadline;
use crate::memo::state_groups::StateGroupExpansionError;
use std::time::Instant;

/// One reader of a kind of shared producer: its memo group and its need.
pub(in crate::memo) struct Reader<N> {
    pub group: usize,
    pub need: N,
}

/// A kind of producer several readers can share.
pub(in crate::memo) trait SharedProducer {
    /// What one reader requires of the producer.
    type Need;
    /// A producer's need that serves several readers at once.
    type Covering;
    /// The memo's readers of this kind, in sets whose members can all read
    /// one producer; in memo order.
    fn sets(
        &mut self,
        memo: &CandidateMemo,
        deadline: Option<Instant>,
    ) -> Result<Vec<Vec<Reader<Self::Need>>>, StateGroupExpansionError>;
    /// The coverings to offer the readers of `set`.
    fn coverings(
        &mut self,
        set: &[Reader<Self::Need>],
        deadline: Option<Instant>,
    ) -> Result<Vec<Self::Covering>, StateGroupExpansionError>;
    /// Adds `reader`'s derivation from the producer at `covering`, if that
    /// producer serves it; the number of candidates added.
    fn derive(
        &mut self,
        memo: &mut CandidateMemo,
        reader: &Reader<Self::Need>,
        covering: &Self::Covering,
    ) -> Result<usize, StateGroupExpansionError>;
}

/// Whether `set`'s readers belong to two or more groups; a set inside one
/// group shares nothing.
pub(in crate::memo) fn spans_groups<N>(set: &[Reader<N>]) -> bool {
    set.iter().any(|reader| reader.group != set[0].group)
}

/// Gives every reader of a set spanning two or more groups its derivation
/// from each of the set's coverings; the number of candidates added.
pub(in crate::memo) fn add_shared_producer_candidates<P: SharedProducer>(
    memo: &mut CandidateMemo,
    producer: &mut P,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let mut added = 0;
    for set in producer.sets(memo, deadline)? {
        check_generation_deadline(deadline)?;
        if !spans_groups(&set) {
            continue;
        }
        let coverings = producer.coverings(&set, deadline)?;
        for reader in &set {
            for covering in &coverings {
                check_generation_deadline(deadline)?;
                added += producer.derive(memo, reader, covering)?;
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}
