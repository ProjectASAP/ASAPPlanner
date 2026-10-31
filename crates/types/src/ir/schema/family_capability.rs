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
//! ([`SketchAlgorithm::capability`]), read unchanged: the family,
//! statistic and merged-error vocabulary is the library's. The planner
//! writes the rows of the exact accumulators, which the library does not
//! have. Which readouts an executor evaluates is its own declaration
//! (`DeploymentCapabilities::supported_summaries`), not this table's.

use super::sketch::{ExactKind, GroupingStrategy, HydraKind, SketchAlgorithm, SketchStatistic};
use crate::ir::schema::FieldDataType;
pub use asap_sketchlib::{MergedError, StatisticKind};

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
    pub(crate) readouts: &'static [(StatisticKind, MergedError)],
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
/// family's guarantee: `asap_sketchlib`'s [`asap_sketchlib::MergeInputRule`]
/// plus [`Self::DisjointInTime`] for the exact counter accumulators.
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

impl From<asap_sketchlib::MergeInputRule> for MergeInputRule {
    fn from(rule: asap_sketchlib::MergeInputRule) -> Self {
        match rule {
            asap_sketchlib::MergeInputRule::Disjoint => Self::Disjoint,
            asap_sketchlib::MergeInputRule::MayOverlap => Self::MayOverlap,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl FamilyCapability {
    /// Each statistic one state of the family answers, with the error of
    /// that readout from a merged state.
    pub fn readouts(&self) -> &'static [(StatisticKind, MergedError)] {
        self.readouts
    }
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
        self.readouts
            .iter()
            .find(|(answered, _)| *answered == kind)
            .map(|&(_, error)| error)
    }

    /// The capability of a per-subpopulation state of `algorithm`, whatever
    /// its parameters: `asap_sketchlib`'s row of the family, which the IR and
    /// a deployment's executor read.
    pub fn of_per_instance_sketch(algorithm: &SketchAlgorithm) -> Self {
        let row = algorithm.capability();
        Self {
            merge_inputs: row.merge_inputs.map(MergeInputRule::from),
            readouts: row.readouts,
        }
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
                return Self::of_per_instance_sketch(&match kind {
                    HydraKind::HydraCms => SketchAlgorithm::HydraCms,
                    HydraKind::HydraCountSketch => SketchAlgorithm::HydraCountSketch,
                    HydraKind::HydraKll => SketchAlgorithm::HydraKll,
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
            readouts,
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
    /// answer top-k and an item's frequency; Count Sketch answers item frequencies, not its total;
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
                vec![SketchStatistic::TopK { k: 10 }, item()],
                vec![SketchStatistic::FrequencyL2, total()],
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
        assert!(FamilyCapability::of(&sum).readouts.is_empty());
        assert!(!FamilyCapability::of(&sum).answers(&total()));
    }
}
