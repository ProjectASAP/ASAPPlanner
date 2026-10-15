//! The generic family-capability table: what each summary family can do,
//! keyed by its [`FieldDataType`].
//!
//! The IR stays permissive: a `SummaryMerge` over any state family is
//! structurally valid. The planner rules and the executor consult this
//! table to decide whether a composition is sound. Today the table holds
//! the mergeability facts; the summary-capability facts (which statistics
//! a family answers) are added as further fields of [`FamilyCapability`].

use super::sketch::{ExactKind, GroupingStrategy, HydraKind, SketchAlgorithm, SketchStatistic};
use crate::pre_asap::schema::FieldDataType;

/// What one summary family can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyCapability {
    /// How states of the family merge; `None` when the family has no sound
    /// merge.
    pub merge: Option<MergeCapability>,
    /// Whether a state subtracts out of another of the same family
    /// (`SummarySubtract`, reserved): the subtrahend's selection must lie
    /// inside the minuend's, and the result is the state of the difference.
    pub subtractable: bool,
}

/// How a family's states merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeCapability {
    /// How the selections of the merged states must relate.
    pub inputs: SelectionRelation,
    /// How the merged state's error relates to its parts' errors.
    pub error: MergedError,
    /// The merged error of readouts that need the family's heavy-hitter
    /// heaps to hold the heavy items (entropy, top-k), when it differs from
    /// `error`. A distinct count reads the heaps only for how many keys
    /// they hold, so it takes `error`.
    pub heap_readouts: Option<MergedError>,
}

/// How the selections of a merge's inputs must relate for the result to
/// keep the family's guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRelation {
    /// No row in two inputs: the family counts a shared row twice.
    Disjoint,
    /// The inputs' time windows do not overlap. Counter states (Rate,
    /// Increase) join consecutive windows in time order, so inputs that are
    /// disjoint only by a value predicate would interleave in time.
    DisjointInTime,
    /// Inputs may share rows: the merge is idempotent.
    OverlapAllowed,
}

/// How a merged state's error relates to its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergedError {
    /// The merged state is exact, as its parts are.
    Exact,
    /// Parts built with the same parameters merge into a state with the
    /// guarantee of one build over the union of their rows: the error does
    /// not grow with the number of parts, so a state sized for the whole
    /// window may be built in parts.
    SameAsUnionBuild,
    /// The merge is allowed, but the merged readout has no guarantee; it is
    /// evidence only, for plan selection to judge.
    Approximate,
}

impl FamilyCapability {
    /// The capability of `family`. Families not listed have no capability
    /// (they fail closed).
    pub fn of(family: &FieldDataType) -> Self {
        use ExactKind as E;
        use SelectionRelation::{Disjoint, DisjointInTime, OverlapAllowed};
        use SketchAlgorithm as S;
        let merge = |inputs, error| {
            Some(MergeCapability {
                inputs,
                error,
                heap_readouts: None,
            })
        };
        // A heavy-hitter heap keeps only the heavy items of its own part:
        // an item heavy over the window but in no single part is lost.
        let with_heaps = |error| {
            Some(MergeCapability {
                inputs: Disjoint,
                error,
                heap_readouts: Some(MergedError::Approximate),
            })
        };
        let (merge, subtractable) = match family {
            FieldDataType::ExactAggregate(kind, _) => match kind {
                E::Sum | E::Count => (merge(Disjoint, MergedError::Exact), true),
                E::Min | E::Max => (merge(OverlapAllowed, MergedError::Exact), false),
                // The counter accumulator joins time-ordered windows with
                // reset handling (`IncreaseAccumulator::merge_pair`).
                E::Rate | E::Increase => (merge(DisjointInTime, MergedError::Exact), false),
                // IRate reads the last two samples of the window, which a
                // pane does not keep across its boundary.
                E::IRate => (None, false),
            },
            // Hydra over a linear sketch is a shared grid of linear
            // sketches: cells add and subtract. Hydra over KLL has no
            // accuracy model.
            FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
                match kind {
                    HydraKind::HydraCms | HydraKind::HydraCountSketch => {
                        (merge(Disjoint, MergedError::SameAsUnionBuild), true)
                    }
                    HydraKind::HydraKll => (None, false),
                }
            }
            FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
                match kind.algorithm() {
                    // Linear sketches: counters add and subtract.
                    S::Cms | S::CountSketch => {
                        (merge(Disjoint, MergedError::SameAsUnionBuild), true)
                    }
                    S::Kll | S::DDSketch => (merge(Disjoint, MergedError::SameAsUnionBuild), false),
                    // Point counts read the linear counters; top-k reads the
                    // heap.
                    S::CmsWithHeap | S::CountSketchWithHeap => {
                        (with_heaps(MergedError::SameAsUnionBuild), false)
                    }
                    // L2 reads layer 0's Count Sketch, whose counters add.
                    // The distinct count's bound needs each layer's heap to
                    // keep `min(|S_j|, k)` of the layer's keys, chosen
                    // without reading the next layer's selection bit;
                    // `UnivMon::merge` re-ranks the union of the parts'
                    // heap keys by the merged sketch, which keeps both, and
                    // its top-layer completeness is the union's. Entropy
                    // and top-k read heavy items a part may have dropped.
                    S::UnivMon => (with_heaps(MergedError::SameAsUnionBuild), false),
                    // `asap_sketchlib`'s KMV hashes every item with one
                    // shared seed (`CANONICAL_HASH_SEED`), and `KMV::merge`
                    // keeps the k smallest distinct hashes: the state of
                    // the union. The executor has no KMV kernel yet (#697).
                    S::Kmv | S::Hll => {
                        (merge(OverlapAllowed, MergedError::SameAsUnionBuild), false)
                    }
                    // Theta is not offered.
                    S::Theta => (None, false),
                }
            }
            FieldDataType::Plain(_)
            | FieldDataType::Sample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => (None, false),
        };
        Self {
            merge,
            subtractable,
        }
    }

    /// The merged error of `readout` from a merged state of this family;
    /// `None` when the family has no sound merge.
    pub fn merged_error(&self, readout: &SketchStatistic) -> Option<MergedError> {
        let merge = self.merge?;
        Some(match (merge.heap_readouts, readout) {
            (Some(heap), SketchStatistic::FrequencyEntropy | SketchStatistic::TopK { .. }) => heap,
            _ => merge.error,
        })
    }
}
