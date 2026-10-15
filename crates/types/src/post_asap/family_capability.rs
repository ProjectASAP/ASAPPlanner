//! The generic family-capability table: what each summary family can do,
//! keyed by its [`FieldDataType`].
//!
//! The IR stays permissive: a `SummaryMerge` over any state family is
//! structurally valid. The planner rules and the executor consult this
//! table to decide whether a composition is sound. One row per family says
//! how the inputs of a merge must relate ([`MergeInputRule`]) and which
//! statistics ([`StatisticKind`]) one state answers, each with the error of
//! that readout from a merged state ([`MergedError`]), so one state can
//! serve every consumer whose statistic it answers.

use super::sketch::{ExactKind, GroupingStrategy, HydraKind, SketchAlgorithm, SketchStatistic};
use crate::pre_asap::schema::FieldDataType;

/// What one summary family can do: its row of the family-capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyCapability {
    /// How the inputs of a merge of the family's states must relate; `None`
    /// when the family has no sound merge.
    pub merge_inputs: Option<MergeInputRule>,
    /// Each statistic one state of the family answers, whatever its
    /// parameters, with the error of that readout from a merged state (read
    /// only when `merge_inputs` is set). Empty for a family read only as a
    /// finalized value (exact accumulators, whose merged state is exact) or
    /// one not listed.
    pub readouts: &'static [(StatisticKind, MergedError)],
}

/// The kind of a `SummaryEstimate` statistic, without its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticKind {
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

impl SketchStatistic {
    /// The kind of statistic this is.
    pub fn kind(&self) -> StatisticKind {
        match self {
            Self::PointCount { value: None, .. } => StatisticKind::Total,
            Self::PointCount { value: Some(_), .. } => StatisticKind::ItemFrequency,
            Self::Quantile { .. } => StatisticKind::Quantile,
            Self::Cardinality => StatisticKind::Distinct,
            Self::FrequencyL2 => StatisticKind::FrequencyL2,
            Self::FrequencyEntropy => StatisticKind::FrequencyEntropy,
            Self::TopK { .. } => StatisticKind::TopK,
        }
    }
}

/// How the inputs of a merge must relate for the result to keep the
/// family's guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeInputRule {
    /// No row in two inputs: the family counts a shared row twice.
    Disjoint,
    /// The inputs' time windows do not overlap. Counter states (Rate,
    /// Increase) join consecutive windows in time order, so inputs that are
    /// disjoint only by a value predicate would interleave in time.
    DisjointInTime,
    /// Inputs may share rows: the merge is idempotent.
    MayOverlap,
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
    AsIfBuiltOverUnion,
    /// The merge is allowed, but the merged readout has no guarantee; it is
    /// evidence only, for plan selection to judge.
    Approximate,
}

impl FamilyCapability {
    /// Whether one state of the family answers `statistic`.
    pub fn answers(&self, statistic: &SketchStatistic) -> bool {
        self.readout(statistic).is_some()
    }

    /// The error of `statistic` read from a merged state of this family;
    /// `None` when the family has no sound merge or does not answer it.
    pub fn merged_error(&self, statistic: &SketchStatistic) -> Option<MergedError> {
        self.merge_inputs?;
        self.readout(statistic)
    }

    /// The table entry of `statistic`'s kind: its merged error.
    fn readout(&self, statistic: &SketchStatistic) -> Option<MergedError> {
        let kind = statistic.kind();
        self.readouts
            .iter()
            .find(|(answered, _)| *answered == kind)
            .map(|&(_, error)| error)
    }

    /// The capability of `family`. Families not listed have no capability
    /// (they fail closed).
    pub fn of(family: &FieldDataType) -> Self {
        use ExactKind as E;
        use MergeInputRule::{Disjoint, DisjointInTime, MayOverlap};
        use MergedError::{Approximate, AsIfBuiltOverUnion as Union};
        use SketchAlgorithm as S;
        use StatisticKind::*;
        let (merge_inputs, readouts): (_, &'static [(StatisticKind, MergedError)]) = match family {
            FieldDataType::ExactAggregate(kind, _) => match kind {
                E::Sum | E::Count => (Some(Disjoint), &[]),
                E::Min | E::Max => (Some(MayOverlap), &[]),
                // The counter accumulator joins time-ordered windows with
                // reset handling (`IncreaseAccumulator::merge_pair`).
                E::Rate | E::Increase => (Some(DisjointInTime), &[]),
                // IRate reads the last two samples of the window, which a
                // pane does not keep across its boundary.
                E::IRate => (None, &[]),
            },
            // Hydra answers per subpopulation, as the inner family does.
            // Over a linear sketch it is a shared grid of linear sketches
            // whose cells add; over KLL it has no accuracy model, so no
            // merge.
            FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
                match kind {
                    HydraKind::HydraCms | HydraKind::HydraCountSketch => {
                        (Some(Disjoint), &[(ItemFrequency, Union)])
                    }
                    HydraKind::HydraKll => (None, &[(Quantile, Approximate)]),
                }
            }
            FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
                match kind.algorithm() {
                    // A quantile sketch answers every rank.
                    S::Kll => (Some(Disjoint), &[(Quantile, Union)]),
                    S::DDSketch => (Some(Disjoint), &[(Quantile, Union), (Total, Union)]),
                    // Linear sketches: counters add. Each Count Sketch row's
                    // squared counters estimate F2 (AMS).
                    S::Cms => (Some(Disjoint), &[(ItemFrequency, Union), (Total, Union)]),
                    S::CountSketch => (
                        Some(Disjoint),
                        &[(ItemFrequency, Union), (FrequencyL2, Union)],
                    ),
                    // Point counts read the linear counters; top-k reads the
                    // heap, which keeps only the heavy items of its own
                    // part: an item heavy over the window but in no single
                    // part is lost.
                    S::CmsWithHeap | S::CountSketchWithHeap => (
                        Some(Disjoint),
                        &[(TopK, Approximate), (ItemFrequency, Union)],
                    ),
                    // One frequency vector serves its total and every
                    // moment. L2 reads layer 0's Count Sketch, whose
                    // counters add. The distinct count's bound needs each
                    // layer's heap to keep `min(|S_j|, k)` of the layer's
                    // keys, chosen without reading the next layer's
                    // selection bit; `UnivMon::merge` re-ranks the union of
                    // the parts' heap keys by the merged sketch, which
                    // keeps both, and its top-layer completeness is the
                    // union's. Entropy reads heavy items a part may have
                    // dropped.
                    S::UnivMon => (
                        Some(Disjoint),
                        &[
                            (Total, Union),
                            (Distinct, Union),
                            (FrequencyL2, Union),
                            (FrequencyEntropy, Approximate),
                        ],
                    ),
                    // `asap_sketchlib`'s KMV hashes every item with one
                    // shared seed (`CANONICAL_HASH_SEED`), and `KMV::merge`
                    // keeps the k smallest distinct hashes: the state of
                    // the union. The executor has no KMV kernel yet (#697).
                    S::Hll | S::Kmv => (Some(MayOverlap), &[(Distinct, Union)]),
                    // Theta is not offered, and has no merge.
                    S::Theta => (None, &[(Distinct, Approximate)]),
                }
            }
            FieldDataType::Plain(_)
            | FieldDataType::Sample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => (None, &[]),
        };
        Self {
            merge_inputs,
            readouts,
        }
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
