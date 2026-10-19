//! The summary families this executor builds, with the readouts it answers
//! from each: what a deployment running it declares as
//! `DeploymentCapabilities::supported_summaries`, so the Planner offers no
//! summary the executor would reject when it binds the plan.

use planner_types::ir::schema::{SketchAlgorithm, StatisticKind, SupportedSummary};

/// Every sketch family this executor builds from rows and the statistics it
/// answers from one state of it, in `SketchAlgorithm` order. A family the
/// executor builds but reads nothing from (plain Count Sketch) and one it
/// does not build (Theta) are absent. Exact accumulators are always
/// supported and are not listed.
///
/// Per-instance states only: Count-Min answers its total (an item's
/// frequency only from a Hydra grid, which Stage 2 does not produce yet;
/// `SupportedSummary` gains a grouping dimension when it does), and the
/// heap families answer top-k only.
pub fn supported_summary_families() -> Vec<SupportedSummary> {
    use SketchAlgorithm as A;
    use StatisticKind::*;
    let families: [(A, &[StatisticKind]); 8] = [
        (
            A::UnivMon,
            &[Total, Distinct, FrequencyL2, FrequencyEntropy],
        ),
        (A::Kll, &[Quantile]),
        (A::Cms, &[Total]),
        (A::Hll, &[Total, Distinct]),
        (A::DDSketch, &[Total, Quantile]),
        (A::CmsWithHeap, &[TopK]),
        (A::Kmv, &[Total, Distinct]),
        (A::CountSketchWithHeap, &[TopK]),
    ];
    families
        .into_iter()
        .map(|(family, readouts)| SupportedSummary {
            family,
            readouts: readouts.to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::validate_sketch_evaluation;
    use crate::factory::create_planner_accumulator;
    use crate::summary_kernels::weighted_frequency::WeightedFrequency;
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{
        FieldDataType, GroupingStrategy, SketchKind, SketchParams, SketchStatistic,
        SummaryInputExpr, SummaryUpdate,
    };

    const ALGORITHMS: [SketchAlgorithm; 10] = [
        SketchAlgorithm::UnivMon,
        SketchAlgorithm::Kll,
        SketchAlgorithm::Cms,
        SketchAlgorithm::Hll,
        SketchAlgorithm::DDSketch,
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::Kmv,
        SketchAlgorithm::Theta,
        SketchAlgorithm::CountSketch,
        SketchAlgorithm::CountSketchWithHeap,
    ];

    const READOUTS: [StatisticKind; 7] = [
        StatisticKind::Total,
        StatisticKind::ItemFrequency,
        StatisticKind::Quantile,
        StatisticKind::Distinct,
        StatisticKind::FrequencyL2,
        StatisticKind::FrequencyEntropy,
        StatisticKind::TopK,
    ];

    /// Valid parameters of `algorithm`, as the Planner sizes them.
    fn params(algorithm: &SketchAlgorithm) -> SketchParams {
        use SketchAlgorithm as A;
        match algorithm {
            A::UnivMon => SketchParams::UnivMon {
                heap_size: 32,
                sketch_rows: 3,
                sketch_cols: 1024,
                layers: 8,
                distinct_failure_probability: None,
            },
            A::Kll => SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
            A::Cms => SketchParams::Cms {
                width: 64,
                depth: 3,
            },
            A::Hll => SketchParams::Hll {
                precision: 12,
                relative_error: None,
            },
            A::DDSketch => SketchParams::DDSketch { alpha: 0.01 },
            A::CmsWithHeap => SketchParams::CmsWithHeap {
                width: 64,
                depth: 3,
                heap_size: 10,
            },
            A::Kmv => SketchParams::Kmv {
                k: 1024,
                relative_error: None,
            },
            A::Theta => SketchParams::Theta { k: 1024 },
            A::CountSketch => SketchParams::CountSketch {
                width: 64,
                depth: 3,
            },
            A::CountSketchWithHeap => SketchParams::CountSketchWithHeap {
                width: 64,
                depth: 3,
                heap_size: 10,
            },
        }
    }

    fn per_instance(algorithm: &SketchAlgorithm) -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(algorithm.clone(), params(algorithm)),
            GroupingStrategy::PerSubpopulationInstance,
        )
    }

    /// A statistic of kind `readout`.
    fn statistic(readout: StatisticKind) -> SketchStatistic {
        let key = ColumnRef::Named("item".into());
        match readout {
            StatisticKind::Total => SketchStatistic::PointCount { key, value: None },
            StatisticKind::ItemFrequency => SketchStatistic::PointCount {
                key,
                value: Some("a".into()),
            },
            StatisticKind::Quantile => SketchStatistic::Quantile { q: 0.99 },
            StatisticKind::Distinct => SketchStatistic::Cardinality,
            StatisticKind::FrequencyL2 => SketchStatistic::FrequencyL2,
            StatisticKind::FrequencyEntropy => SketchStatistic::FrequencyEntropy,
            StatisticKind::TopK => SketchStatistic::TopK { k: 5 },
        }
    }

    /// Whether the factory builds the per-instance `family` from rows, with
    /// a value column or keyed items.
    fn factory_builds(family: &FieldDataType) -> bool {
        let FieldDataType::Sketch(_, grouping) = family else {
            return false;
        };
        let value = SummaryUpdate::column(ColumnRef::SampleValue);
        let keyed = SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::Named("item".into()))),
            ..value.clone()
        };
        create_planner_accumulator(family, &value, grouping).is_ok()
            || create_planner_accumulator(family, &keyed, grouping).is_ok()
    }

    /// Whether the executor answers `readout` from `family`'s state: top-k
    /// through the keyed heap evaluation, every other readout through the
    /// sketch evaluation.
    fn evaluation_answers(family: &FieldDataType, readout: StatisticKind) -> bool {
        match (readout, family) {
            (StatisticKind::TopK, FieldDataType::Sketch(kind, _)) => {
                WeightedFrequency::configuration(kind).is_ok()
            }
            _ => validate_sketch_evaluation(family, &statistic(readout)).is_ok(),
        }
    }

    /// The list holds exactly the (family, readout) pairs the factory builds
    /// per instance and an evaluation answers from that state.
    #[test]
    fn supported_summary_families_match_the_factory_and_evaluations() {
        let listed = supported_summary_families();
        for algorithm in &ALGORITHMS {
            // Per instance: Stage 1 builds every family per instance; the
            // Hydra grid is not produced outside tests yet.
            let family = per_instance(algorithm);
            for readout in READOUTS {
                let executed = factory_builds(&family) && evaluation_answers(&family, readout);
                let declared = listed
                    .iter()
                    .any(|s| &s.family == algorithm && s.readouts.contains(&readout));
                assert_eq!(declared, executed, "{algorithm:?} {readout:?}");
            }
        }
        assert!(listed.iter().all(|s| !s.readouts.is_empty()));
    }
}
