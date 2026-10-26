//! The summary families this executor builds, with the readouts it answers
//! from each: what a deployment running it declares as
//! `DeploymentCapabilities::supported_summaries`, so the Planner offers no
//! summary the executor would reject when it binds the plan.

use crate::capability::{EVALUATED_FAMILIES, UNEVALUATED_READOUTS};
use planner_types::ir::schema::{FamilyCapability, SupportedSummary};

/// Every sketch family this executor builds from rows and the statistics it
/// answers from one state of it: the family-capability table's row of each
/// family it evaluates, less the readouts it does not evaluate yet. Exact
/// accumulators are always supported and are not listed.
pub fn supported_summary_families() -> Vec<SupportedSummary> {
    EVALUATED_FAMILIES
        .into_iter()
        .map(|family| SupportedSummary {
            readouts: FamilyCapability::of_per_instance_sketch(&family)
                .readouts
                .iter()
                .map(|&(readout, _)| readout)
                .filter(|readout| !UNEVALUATED_READOUTS.contains(&(family.clone(), *readout)))
                .collect(),
            family,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::validate_sketch_evaluation;
    use crate::factory::create_planner_accumulator;
    use crate::summary_kernels::weighted_frequency::WeightedFrequency;
    use planner_types::ir::operator::operator_properties::{Reduction, Source};
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{
        DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
        SketchParams, SketchStatistic, StatisticKind, SummaryInputExpr, SummaryUpdate,
    };
    use planner_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};

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
            // Pass 1 reads a total keyed by the sample value.
            StatisticKind::Total => SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
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
    /// sketch evaluation, whose kernel must then read it from a built state.
    fn evaluation_answers(family: &FieldDataType, readout: StatisticKind) -> bool {
        match (readout, family) {
            (StatisticKind::TopK, FieldDataType::Sketch(kind, _)) => {
                WeightedFrequency::configuration(kind).is_ok()
            }
            _ => {
                let accepted = validate_sketch_evaluation(family, &statistic(readout)).is_ok();
                if accepted {
                    assert!(kernel_answers(family, readout), "{family:?} {readout:?}");
                }
                accepted
            }
        }
    }

    /// Whether the kernel of a per-instance `family` state built from one
    /// update reads `readout`.
    fn kernel_answers(family: &FieldDataType, readout: StatisticKind) -> bool {
        let FieldDataType::Sketch(_, grouping) = family else {
            return false;
        };
        let value = SummaryUpdate::column(ColumnRef::SampleValue);
        let keyed = SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::Named("item".into()))),
            ..value.clone()
        };
        let Ok(mut updater) = create_planner_accumulator(family, &value, grouping)
            .or_else(|_| create_planner_accumulator(family, &keyed, grouping))
        else {
            return false;
        };
        let updated = if updater.is_keyed() {
            updater.update_keyed(
                &crate::KeyByLabelValues::new_with_labels(vec!["a".into()]),
                1.0,
                0,
            )
        } else {
            updater.update_single(1.0, 0)
        };
        updated.is_ok()
            && updater
                .snapshot_accumulator()
                .estimate(&statistic(readout))
                .is_ok()
    }

    /// Whether the IR builds a structurally valid `SummaryEstimate` of
    /// `readout` over a `family` state.
    fn ir_builds_estimate(family: &FieldDataType, readout: StatisticKind) -> bool {
        let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![
                Field::plain("item", DataType::Utf8, false),
                Field::plain("value", DataType::Float64, false),
            ]),
        }))
        .unwrap();
        let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan,
            family: family.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![]),
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap();
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: statistic(readout),
        }))
        .is_ok_and(|estimate| estimate.validate_structure().is_ok())
    }

    /// The list holds exactly the (family, readout) pairs the factory builds
    /// per instance and an evaluation answers from that state, and the
    /// IR's `SummaryEstimate` validation accepts exactly what the
    /// family-capability table answers. A listed family's readouts are its
    /// table row less the ones named not evaluated, each of which the table
    /// answers and the executor does not.
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
                let answered = FamilyCapability::of(&family).answers(&statistic(readout));
                assert_eq!(
                    ir_builds_estimate(&family, readout),
                    answered,
                    "IR: {algorithm:?} {readout:?}"
                );
                if UNEVALUATED_READOUTS.contains(&(algorithm.clone(), readout)) {
                    assert!(answered && !executed, "{algorithm:?} {readout:?}");
                } else if EVALUATED_FAMILIES.contains(algorithm) {
                    assert_eq!(answered, executed, "table: {algorithm:?} {readout:?}");
                }
            }
        }
        assert!(listed.iter().all(|s| !s.readouts.is_empty()));
    }
}
