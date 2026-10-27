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
    /// Every reader of this kind among the memo's candidates, in memo order.
    fn readers(
        &mut self,
        memo: &CandidateMemo,
        deadline: Option<Instant>,
    ) -> Result<Vec<Reader<Self::Need>>, StateGroupExpansionError>;
    /// Whether two readers can read one producer.
    fn can_share(&self, a: &Reader<Self::Need>, b: &Reader<Self::Need>) -> bool;
    /// The coverings to offer the readers `sharing`, which can all share.
    fn covering(
        &mut self,
        memo: &CandidateMemo,
        sharing: &[Reader<Self::Need>],
        deadline: Option<Instant>,
    ) -> Result<Vec<Self::Covering>, StateGroupExpansionError>;
    /// Whether a producer at `covering` serves `reader`.
    fn serves(&self, covering: &Self::Covering, reader: &Reader<Self::Need>) -> bool;
    /// Adds `reader`'s derivation from the producer at `covering`, one of
    /// `sharing`'s coverings; the number of candidates added.
    fn derive(
        &mut self,
        memo: &mut CandidateMemo,
        sharing: &[Reader<Self::Need>],
        reader: &Reader<Self::Need>,
        covering: &Self::Covering,
    ) -> Result<usize, StateGroupExpansionError>;
}

/// Groups `producer`'s readers into sets that can share, and gives every
/// reader of a set spanning two or more groups its derivation from each
/// covering that serves it; the number of candidates added.
pub(in crate::memo) fn add_shared_producer_candidates<P: SharedProducer>(
    memo: &mut CandidateMemo,
    producer: &mut P,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    let mut sets: Vec<Vec<Reader<P::Need>>> = Vec::new();
    for reader in producer.readers(memo, deadline)? {
        match sets
            .iter_mut()
            .find(|set| producer.can_share(&set[0], &reader))
        {
            Some(set) => set.push(reader),
            None => sets.push(vec![reader]),
        }
    }
    let mut added = 0;
    for sharing in &sets {
        check_generation_deadline(deadline)?;
        if !sharing
            .iter()
            .any(|reader| reader.group != sharing[0].group)
        {
            continue;
        }
        let coverings = producer.covering(memo, sharing, deadline)?;
        for reader in sharing {
            for covering in &coverings {
                check_generation_deadline(deadline)?;
                if producer.serves(covering, reader) {
                    added += producer.derive(memo, sharing, reader, covering)?;
                }
            }
        }
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}
