//! The generic family-capability table: what each summary family can do,
//! keyed by its [`FieldDataType`].
//!
//! The IR stays permissive: a `SummaryMerge` over any state family is
//! structurally valid. The planner rules and the executor consult this
//! table to decide whether a composition is sound. It holds the
//! mergeability facts and the summary-capability facts: which readouts
//! ([`Readout`]) one state of the family answers, so one state can serve
//! every consumer whose statistic it answers.

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
    /// The readouts one state of the family answers, whatever its
    /// parameters; none for a family read only as a finalized value (exact
    /// accumulators) or one not listed.
    pub readouts: &'static [Readout],
}

/// One kind of readout (`SummaryEstimate` statistic) of a summary state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readout {
    /// The total of the updates: a bare `PointCount` (no item value).
    Total,
    /// One item's frequency: a `PointCount` with an item value.
    ItemFrequency,
    /// The value at any quantile rank.
    Quantile,
    /// The number of distinct items.
    Distinct,
    /// The L2 norm of the item frequencies.
    FrequencyL2,
    /// The entropy of the item frequencies.
    FrequencyEntropy,
    /// The heaviest items, any `k` up to the state's heap.
    TopK,
}

impl Readout {
    /// The kind of readout `statistic` is.
    pub fn of(statistic: &SketchStatistic) -> Self {
        match statistic {
            SketchStatistic::PointCount { value: None, .. } => Self::Total,
            SketchStatistic::PointCount { value: Some(_), .. } => Self::ItemFrequency,
            SketchStatistic::Quantile { .. } => Self::Quantile,
            SketchStatistic::Cardinality => Self::Distinct,
            SketchStatistic::FrequencyL2 => Self::FrequencyL2,
            SketchStatistic::FrequencyEntropy => Self::FrequencyEntropy,
            SketchStatistic::TopK { .. } => Self::TopK,
        }
    }
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
    /// Whether one state of the family answers `statistic`.
    pub fn answers(&self, statistic: &SketchStatistic) -> bool {
        self.readouts.contains(&Readout::of(statistic))
    }

    /// The capability of `family`. Families not listed have no capability
    /// (they fail closed).
    pub fn of(family: &FieldDataType) -> Self {
        Self {
            readouts: readouts(family),
            ..Self::merge_facts(family)
        }
    }

    fn merge_facts(family: &FieldDataType) -> Self {
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
            readouts: &[],
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

/// The readouts one state of `family` answers.
fn readouts(family: &FieldDataType) -> &'static [Readout] {
    use Readout::*;
    use SketchAlgorithm as S;
    match family {
        FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
            match kind {
                // Per subpopulation, as the inner family answers.
                HydraKind::HydraCms | HydraKind::HydraCountSketch => &[ItemFrequency],
                HydraKind::HydraKll => &[Quantile],
            }
        }
        FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
            match kind.algorithm() {
                // One frequency vector serves its total and every moment.
                S::UnivMon => &[Total, Distinct, FrequencyL2, FrequencyEntropy],
                // A quantile sketch answers every rank.
                S::Kll => &[Quantile],
                S::DDSketch => &[Quantile, Total],
                S::Cms => &[ItemFrequency, Total],
                // Each row's squared counters estimate F2 (AMS).
                S::CountSketch => &[ItemFrequency, FrequencyL2],
                S::CmsWithHeap | S::CountSketchWithHeap => &[TopK, ItemFrequency],
                S::Hll | S::Kmv | S::Theta => &[Distinct],
            }
        }
        FieldDataType::ExactAggregate(..)
        | FieldDataType::Plain(_)
        | FieldDataType::Sample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_asap::{ExactParams, SketchKind, SketchParams};
    use crate::pre_asap::expr_ir::ColumnRef;

    fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(algorithm, params),
            GroupingStrategy::default(),
        )
    }

    fn total() -> SketchStatistic {
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        }
    }

    fn item() -> SketchStatistic {
        SketchStatistic::PointCount {
            key: ColumnRef::Named("src_ip".into()),
            value: Some("10.0.0.1".into()),
        }
    }

    /// One UnivMon answers its total, the distinct count, L2 and entropy,
    /// so it can serve all four; it answers no quantile or top-k.
    #[test]
    fn univmon_answers_its_total_and_every_frequency_moment() {
        let univmon = FamilyCapability::of(&sketch(
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size: 256,
                sketch_rows: 5,
                sketch_cols: 1024,
                layers: 16,
                distinct_failure_probability: None,
            },
        ));
        for statistic in [
            total(),
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyL2,
            SketchStatistic::FrequencyEntropy,
        ] {
            assert!(univmon.answers(&statistic), "{statistic:?}");
        }
        for statistic in [
            SketchStatistic::Quantile { q: 0.5 },
            SketchStatistic::TopK { k: 3 },
        ] {
            assert!(!univmon.answers(&statistic), "{statistic:?}");
        }
    }

    /// A quantile sketch answers every rank; heap sketches answer top-k and
    /// item frequencies; CountSketch answers item frequencies and L2.
    #[test]
    fn summary_capability_facts() {
        let kll = sketch(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        );
        let count_sketch = sketch(
            SketchAlgorithm::CountSketch,
            SketchParams::CountSketch {
                width: 64,
                depth: 5,
            },
        );
        let cms_heap = sketch(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 64,
                depth: 5,
                heap_size: 10,
            },
        );
        let hll = sketch(
            SketchAlgorithm::Hll,
            SketchParams::Hll {
                precision: 12,
                relative_error: None,
            },
        );
        for (family, yes, no) in [
            (
                &kll,
                vec![
                    SketchStatistic::Quantile { q: 0.5 },
                    SketchStatistic::Quantile { q: 0.99 },
                ],
                vec![SketchStatistic::Cardinality, total()],
            ),
            (
                &count_sketch,
                vec![item(), SketchStatistic::FrequencyL2],
                vec![
                    SketchStatistic::FrequencyEntropy,
                    SketchStatistic::TopK { k: 3 },
                ],
            ),
            (
                &cms_heap,
                vec![SketchStatistic::TopK { k: 10 }, item()],
                vec![SketchStatistic::FrequencyL2],
            ),
            (
                &hll,
                vec![SketchStatistic::Cardinality],
                vec![SketchStatistic::FrequencyL2, item()],
            ),
        ] {
            let capability = FamilyCapability::of(family);
            for statistic in yes {
                assert!(capability.answers(&statistic), "{family:?} {statistic:?}");
            }
            for statistic in no {
                assert!(!capability.answers(&statistic), "{family:?} {statistic:?}");
            }
        }
    }

    /// An exact accumulator is read as its finalized value, not a readout.
    #[test]
    fn exact_accumulators_answer_no_readout() {
        let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        assert!(FamilyCapability::of(&sum).readouts.is_empty());
        assert!(!FamilyCapability::of(&sum).answers(&total()));
    }
}
