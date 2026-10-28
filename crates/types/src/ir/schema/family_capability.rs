//! The generic family-capability table: what each summary family can do,
//! keyed by its [`FieldDataType`].
//!
//! The IR stays permissive: a `SummaryMerge` over any state family is
//! structurally valid. The planner rules and the executor consult this
//! table to decide whether a composition is sound. It is the one source of
//! which statistic a family answers: the IR's `SummaryEstimate` validation,
//! the executor's evaluation check and its supported-summary list all read
//! it. One row per family says
//! how the inputs of a merge must relate ([`MergeInputRule`]) and which
//! statistics ([`StatisticKind`]) one state answers, each with the error of
//! that readout from a merged state ([`MergedError`]), so one state can
//! serve every consumer whose statistic it answers.
//!
//! A sketch family's row is `asap_sketchlib`'s
//! ([`SketchFamily::capability`]), read in this table's vocabulary, less the
//! readouts this stack does not read yet (`NOT_READ_YET`). The planner
//! writes the rows of the families the library has none for: the exact
//! accumulators and Theta.

use super::sketch::{ExactKind, GroupingStrategy, HydraKind, SketchAlgorithm, SketchStatistic};
use crate::ir::schema::FieldDataType;
use asap_sketchlib::{MergeInputs, SketchFamily, Statistic};

/// What one summary family can do: its row of the family-capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyCapability {
    /// How the inputs of a merge of the family's states must relate; `None`
    /// when the family has no sound merge.
    pub merge_inputs: Option<MergeInputRule>,
    /// Where the row's readouts ([`Self::readouts`]) come from.
    readouts: Readouts,
}

/// Where a row's readouts come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readouts {
    /// `asap_sketchlib`'s row of the family.
    Sketchlib(SketchFamily),
    /// The planner's own row, for a family the library has none for.
    Planner(&'static [(StatisticKind, MergedError)]),
}

/// Readouts `asap_sketchlib` lists that the planner's table did not list
/// before it read the library's rows, and that nothing in the planner or
/// the executor reads yet: the table leaves them out. A readout the
/// planner's table listed stays even where the executor does not evaluate
/// it (a per-instance Count-Min's item frequency).
const NOT_READ_YET: &[(SketchFamily, Statistic)] = &[
    (SketchFamily::DdSketch, Statistic::Total),
    (SketchFamily::CountMinWithHeap, Statistic::ItemFrequency),
    (SketchFamily::CountSketchWithHeap, Statistic::ItemFrequency),
    (SketchFamily::HydraCountMin, Statistic::Total),
];

/// `family`'s readouts in `asap_sketchlib`'s table, in this table's
/// vocabulary: the statistics it has a kind for, less [`NOT_READ_YET`].
fn sketchlib_readouts(family: SketchFamily) -> impl Iterator<Item = (StatisticKind, MergedError)> {
    use asap_sketchlib::MergedError as M;
    family
        .capability()
        .readouts
        .iter()
        .filter(move |&&(statistic, _)| !NOT_READ_YET.contains(&(family, statistic)))
        .filter_map(|&(statistic, error)| {
            let kind = match statistic {
                Statistic::Total => StatisticKind::Total,
                Statistic::ItemFrequency => StatisticKind::ItemFrequency,
                Statistic::Quantile => StatisticKind::Quantile,
                Statistic::Distinct => StatisticKind::Distinct,
                Statistic::FrequencyL2 => StatisticKind::FrequencyL2,
                Statistic::FrequencyEntropy => StatisticKind::FrequencyEntropy,
                Statistic::TopK => StatisticKind::TopK,
                _ => return None,
            };
            let error = match error {
                M::Exact => MergedError::Exact,
                M::AsIfBuiltOverUnion => MergedError::AsIfBuiltOverUnion,
                M::NoGuarantee => MergedError::Approximate,
            };
            Some((kind, error))
        })
}

/// The kind of a `SummaryEstimate` statistic, without its arguments: the
/// readout vocabulary of this table and of a deployment's supported
/// summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// One sketch family a deployment's executor can build, with the readouts
/// it can answer from the family's state: an entry of
/// `DeploymentCapabilities::supported_summaries`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportedSummary {
    /// The family the executor can build.
    pub family: SketchAlgorithm,
    /// The statistics it can answer from one state of the family.
    pub readouts: Vec<StatisticKind>,
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
    /// Whether a rollup may merge different groups' states of `family`: its
    /// merge takes disjoint or overlapping inputs, and each group has its own
    /// state. Counter states merge consecutive windows of one series; a
    /// family without a merge merges nothing; a Hydra's groups share one
    /// state.
    pub fn merges_groups(family: &FieldDataType) -> bool {
        matches!(
            Self::of(family).merge_inputs,
            Some(MergeInputRule::Disjoint | MergeInputRule::MayOverlap)
        ) && !matches!(
            family,
            FieldDataType::Sketch(_, grouping)
                if *grouping != GroupingStrategy::PerSubpopulationInstance
        )
    }

    /// Each statistic one state of the family answers, whatever its
    /// parameters, with the error of that readout from a merged state (read
    /// only when `merge_inputs` is set). None for a family read only as a
    /// finalized value (exact accumulators, whose merged state is exact) or
    /// one not listed.
    pub fn readouts(&self) -> impl Iterator<Item = (StatisticKind, MergedError)> {
        let (planner, family) = match self.readouts {
            Readouts::Planner(readouts) => (readouts, None),
            Readouts::Sketchlib(family) => (&[][..], Some(family)),
        };
        planner
            .iter()
            .copied()
            .chain(family.into_iter().flat_map(sketchlib_readouts))
    }

    /// Whether one state of the family answers `statistic`.
    pub fn answers(&self, statistic: &SketchStatistic) -> bool {
        self.readout(statistic).is_some()
    }

    /// The error of `statistic` read from a merged state of this family;
    /// `None` when the family has no sound merge or does not answer
    /// `statistic`.
    pub fn merged_error(&self, statistic: &SketchStatistic) -> Option<MergedError> {
        self.merge_inputs.and(self.readout(statistic))
    }

    /// The table entry of `statistic`'s kind: its merged error.
    fn readout(&self, statistic: &SketchStatistic) -> Option<MergedError> {
        let kind = statistic.kind();
        self.readouts()
            .find(|(answered, _)| *answered == kind)
            .map(|(_, error)| error)
    }

    /// `asap_sketchlib`'s row of `family`.
    fn of_sketchlib(family: SketchFamily) -> Self {
        Self {
            merge_inputs: family.capability().merge.map(|merge| match merge {
                MergeInputs::Disjoint => MergeInputRule::Disjoint,
                MergeInputs::MayOverlap => MergeInputRule::MayOverlap,
            }),
            readouts: Readouts::Sketchlib(family),
        }
    }

    /// The capability of a per-subpopulation state of `algorithm`, whatever
    /// its parameters: the table row the IR and a deployment's executor
    /// read for that family.
    pub fn of_per_instance_sketch(algorithm: &SketchAlgorithm) -> Self {
        use SketchAlgorithm as S;
        Self::of_sketchlib(match algorithm {
            S::Kll => SketchFamily::Kll,
            S::DDSketch => SketchFamily::DdSketch,
            S::Cms => SketchFamily::CountMin,
            S::CountSketch => SketchFamily::CountSketch,
            S::CmsWithHeap => SketchFamily::CountMinWithHeap,
            S::CountSketchWithHeap => SketchFamily::CountSketchWithHeap,
            S::UnivMon => SketchFamily::UnivMon,
            S::Hll => SketchFamily::Hll,
            S::Kmv => SketchFamily::Kmv,
            // Theta is not offered, and has no merge.
            S::Theta => {
                return Self {
                    merge_inputs: None,
                    readouts: Readouts::Planner(&[(
                        StatisticKind::Distinct,
                        MergedError::Approximate,
                    )]),
                }
            }
        })
    }

    /// The capability of `family`. Families not listed have no capability
    /// (they fail closed).
    pub fn of(family: &FieldDataType) -> Self {
        use ExactKind as E;
        use MergeInputRule::{Disjoint, DisjointInTime, MayOverlap};
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
            // A Hydra answers per subpopulation from a shared grid of the
            // inner family's cells. HydraKll merges as sketchlib's row says:
            // `Hydra::merge` requires the same k and m and otherwise returns
            // an error (asap_sketchlib #251). Nothing builds one yet: Pass 1
            // offers no Hydra, and the executor builds only HydraCms
            // (`create_hydra_cms`).
            FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
                return Self::of_sketchlib(match kind {
                    HydraKind::HydraCms => SketchFamily::HydraCountMin,
                    HydraKind::HydraCountSketch => SketchFamily::HydraCountSketch,
                    HydraKind::HydraKll => SketchFamily::HydraKll,
                })
            }
            FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
                return Self::of_per_instance_sketch(kind.algorithm())
            }
            FieldDataType::Plain(_)
            | FieldDataType::SummarySample(..)
            | FieldDataType::Wavelet(..)
            | FieldDataType::StatModel(..) => (None, &[]),
        };
        Self {
            merge_inputs,
            readouts: Readouts::Planner(readouts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{ExactParams, SketchKind, SketchParams};

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

    /// A quantile sketch answers every rank, not its total; heap sketches
    /// answer top-k; Count Sketch answers item frequencies, not its total;
    /// HLL answers the distinct count, not its total.
    #[test]
    fn summary_capability_facts() {
        let kll = sketch(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        );
        let ddsketch = sketch(
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
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
                &ddsketch,
                vec![SketchStatistic::Quantile { q: 0.9 }],
                vec![total()],
            ),
            (
                &count_sketch,
                vec![item()],
                vec![
                    total(),
                    SketchStatistic::FrequencyL2,
                    SketchStatistic::FrequencyEntropy,
                    SketchStatistic::TopK { k: 3 },
                ],
            ),
            (
                &cms_heap,
                vec![SketchStatistic::TopK { k: 10 }],
                vec![SketchStatistic::FrequencyL2, item()],
            ),
            (
                &hll,
                vec![SketchStatistic::Cardinality],
                vec![SketchStatistic::FrequencyL2, item(), total()],
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

    /// A rollup merges different groups' states only of a family whose
    /// merge takes disjoint or overlapping inputs and that keeps one state
    /// per group: not counters, IRate or a Hydra.
    #[test]
    fn only_per_group_mergeable_families_merge_groups() {
        use crate::ir::schema::{HydraKind, HydraParams};
        let kll = sketch(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        );
        assert!(FamilyCapability::merges_groups(&kll));
        assert!(FamilyCapability::merges_groups(
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        ));
        for (kind, params) in [
            (ExactKind::Rate, ExactParams::Rate),
            (ExactKind::Increase, ExactParams::Increase),
            (ExactKind::IRate, ExactParams::IRate),
        ] {
            assert!(!FamilyCapability::merges_groups(
                &FieldDataType::ExactAggregate(kind, params)
            ));
        }
        let FieldDataType::Sketch(kind, _) = sketch(
            SketchAlgorithm::Cms,
            SketchParams::Cms {
                width: 64,
                depth: 5,
            },
        ) else {
            unreachable!()
        };
        let hydra = FieldDataType::Sketch(
            kind,
            GroupingStrategy::SharedMultiSubpopulation {
                kind: HydraKind::HydraCms,
                params: HydraParams::HydraCms {
                    width: 64,
                    depth: 5,
                    shared_rows: 3,
                    shared_columns: 256,
                },
            },
        );
        assert_eq!(
            FamilyCapability::of(&hydra).merge_inputs,
            Some(MergeInputRule::Disjoint)
        );
        assert!(!FamilyCapability::merges_groups(&hydra));
    }

    /// An exact accumulator is read as its finalized value, not a readout.
    #[test]
    fn exact_accumulators_answer_no_readout() {
        let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        assert_eq!(FamilyCapability::of(&sum).readouts().next(), None);
        assert!(!FamilyCapability::of(&sum).answers(&total()));
    }

    /// Every readout kept out as not read yet is one `asap_sketchlib`
    /// still lists, so the list holds no stale entry.
    #[test]
    fn readouts_not_read_yet_are_listed_by_sketchlib() {
        for &(family, statistic) in NOT_READ_YET {
            assert!(
                family.capability().answers(statistic),
                "{family:?} {statistic:?}"
            );
        }
    }
}
