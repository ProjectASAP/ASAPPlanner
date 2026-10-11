//! Capability boundaries, checked without constructing accumulator state.
//!
//! `validate_summary_kernel` checks update kernels, including families without a
//! native batch representation. `validate_native_family` and
//! `validate_sketch_evaluation` / `validate_exact_evaluation` check native state and evaluation support.
//! Keyed weighted-frequency evaluations are checked by `Operator::keyed_evaluation`.
//! A successful kernel check alone does not mean a physical DAG will bind.
//!
//! Stored-state encodings belong to deployments. Full plan acceptance is
//! owned by `binding`, which also validates schemas, expressions and inputs.
//!
//! [`capabilities`] states these rules as the planner's deployment input.
use crate::Error;
use planner_types::ir::schema::{
    ExactKind, ExactParams, FieldDataType as SummaryFamilyType, GroupingStrategy, SketchAlgorithm,
    SketchParams, SketchStatistic, SummaryUpdate,
};

/// Check the same contract used by `create_planner_accumulator` before a plan
/// is accepted. Execution timing is deliberately not a kernel property.
pub fn validate_summary_kernel(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
    grouping: &GroupingStrategy,
) -> Result<(), String> {
    if grouping != &GroupingStrategy::PerSubpopulationInstance {
        return validate_hydra_cms(family, input, grouping);
    }
    let keyed = match family {
        SummaryFamilyType::ExactAggregate(kind, params) => {
            use ExactKind as K;
            use ExactParams as P;
            if !matches!(
                (kind, params),
                (K::Sum, P::Sum)
                    | (K::Count, P::Count)
                    | (K::Min, P::Min)
                    | (K::Max, P::Max)
                    | (K::Rate, P::Rate)
                    | (K::Increase, P::Increase)
            ) {
                return Err(format!("unsupported exact kernel {family:?}"));
            }
            input.item.is_some()
        }
        SummaryFamilyType::Sketch(kind, layout) => {
            if layout != grouping {
                return Err("Planner family and operator grouping disagree".into());
            }
            use SketchAlgorithm as A;
            use SketchParams as P;
            match (kind.algorithm(), kind.params()) {
                (A::Kll, P::Kll { k }) if (8..=u16::MAX as u32).contains(k) => false,
                (A::DDSketch, P::DDSketch { alpha })
                    if alpha.is_finite() && *alpha > 0.0 && *alpha < 1.0 =>
                {
                    false
                }
                (A::Hll, P::Hll { precision }) if (4..=18).contains(precision) => false,
                (A::Cms, P::Cms { width, depth })
                | (A::CountSketch, P::CountSketch { width, depth })
                    if valid_matrix(*width, *depth) =>
                {
                    true
                }
                (
                    A::CmsWithHeap,
                    P::CmsWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
                )
                | (
                    A::CountSketchWithHeap,
                    P::CountSketchWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
                ) if valid_matrix(*width, *depth) && *heap_size > 0 => true,
                (
                    A::UnivMon,
                    P::UnivMon {
                        heap_size,
                        sketch_rows,
                        sketch_cols,
                        layers,
                    },
                ) if *heap_size > 0
                    && *sketch_cols > 0
                    && (1..=20).contains(sketch_rows)
                    && (1..=64).contains(layers)
                    && (*sketch_rows as usize)
                        .checked_mul(*sketch_cols as usize)
                        .and_then(|n| n.checked_mul(*layers as usize))
                        .is_some() =>
                {
                    false
                }
                _ => {
                    return Err(format!(
                        "unsupported kernel or invalid parameters: {kind:?}"
                    ))
                }
            }
        }
        _ => return Err(format!("unsupported summary kernel {family:?}")),
    };
    if keyed != input.item.is_some() && !is_unit_sample_frequency(input) {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

/// The only shared grouping with a kernel: Hydra over Count-Min. Each update
/// adds a unit or non-negative column weight for one item of one group.
fn validate_hydra_cms(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
    grouping: &GroupingStrategy,
) -> Result<(), String> {
    use planner_types::ir::schema::{SummaryInputExpr, WeightDomain};
    hydra_cms_shape(family, grouping)?;
    if !matches!(input.item, Some(SummaryInputExpr::Column(_))) {
        return Err("HydraCms requires one item column".into());
    }
    if !matches!(input.weight_domain, WeightDomain::NonNegative { .. })
        || !matches!(
            input.weight,
            SummaryInputExpr::Constant(1.0) | SummaryInputExpr::Column(_)
        )
    {
        return Err("HydraCms requires a unit or non-negative column weight".into());
    }
    Ok(())
}

/// `(shared_rows, shared_columns, width, depth)` of a HydraCms state. Its
/// family is the per-group Count-Min it emulates, with the Hydra's own width
/// and depth.
pub(crate) fn hydra_cms_shape(
    family: &SummaryFamilyType,
    grouping: &GroupingStrategy,
) -> Result<(usize, usize, usize, usize), String> {
    use planner_types::ir::schema::{HydraKind, HydraParams};
    let GroupingStrategy::SharedMultiSubpopulation {
        kind: HydraKind::HydraCms,
        params:
            HydraParams::HydraCms {
                width,
                depth,
                shared_rows,
                shared_columns,
            },
    } = grouping
    else {
        return Err("shared summary grouping has no registered kernel".into());
    };
    let SummaryFamilyType::Sketch(kind, layout) = family else {
        return Err("HydraCms requires a Count-Min family".into());
    };
    if layout != grouping {
        return Err("Planner family and operator grouping disagree".into());
    }
    if kind.algorithm() != &SketchAlgorithm::Cms
        || kind.params()
            != &(SketchParams::Cms {
                width: *width,
                depth: *depth,
            })
    {
        return Err("HydraCms family must be Count-Min with the Hydra width and depth".into());
    }
    if !valid_matrix(*width, *depth) || !valid_matrix(*shared_columns, *shared_rows) {
        return Err("invalid HydraCms dimensions".into());
    }
    Ok((
        *shared_rows as usize,
        *shared_columns as usize,
        *width as usize,
        *depth as usize,
    ))
}

fn valid_matrix(width: u32, depth: u32) -> bool {
    // Construction uses the kernel's native row hashing, so no encoded-size
    // limit applies here.
    width > 0
        && depth > 0
        && (width as usize)
            .checked_mul(depth as usize)
            .and_then(|n| n.checked_mul(std::mem::size_of::<f64>()))
            .is_some()
}

pub(crate) fn is_unit_sample_frequency(update: &planner_types::ir::schema::SummaryUpdate) -> bool {
    use planner_types::ir::schema::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    matches!(
        update.item,
        Some(SummaryInputExpr::Column(
            planner_types::ir::scalar::ColumnRef::SampleValue
        ))
    ) && matches!(update.weight, SummaryInputExpr::Constant(1.0))
        && matches!(
            update.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount
            }
        )
}

pub fn validate_native_family(family: &SummaryFamilyType) -> Result<(), Error> {
    use planner_types::ir::schema::SketchAlgorithm as A;
    if let SummaryFamilyType::Sketch(kind, grouping) = family {
        // Plain Count-Min is native as stored state only: it merges and reads
        // its bare count, but the DAG does not build it from rows. HydraCms
        // is built by the shared summary operator.
        if let (A::Cms, SketchParams::Cms { width, depth }) = (kind.algorithm(), kind.params()) {
            if grouping != &GroupingStrategy::PerSubpopulationInstance {
                return hydra_cms_shape(family, grouping)
                    .map(|_| ())
                    .map_err(Error::Invalid);
            }
            return if valid_matrix(*width, *depth) {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "invalid Count-Min dimensions or grouping strategy".into(),
                ))
            };
        }
        if matches!(kind.algorithm(), A::CmsWithHeap | A::CountSketchWithHeap) {
            let (_, width, depth, _) =
                crate::summary_kernels::weighted_frequency::WeightedFrequency::configuration(kind)?;
            return if valid_matrix(width as u32, depth as u32) && grouping == &Default::default() {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "invalid weighted frequency dimensions or grouping strategy".into(),
                ))
            };
        }
    }
    match family {
        SummaryFamilyType::ExactAggregate(..) => {}
        SummaryFamilyType::Sketch(kind, _)
            if matches!(kind.algorithm(), A::Kll | A::DDSketch | A::Hll | A::UnivMon) => {}
        _ => {
            return Err(Error::Invalid(
                "summary family has no native DAG state implementation".into(),
            ))
        }
    }
    crate::capability::validate_summary_kernel(
        family,
        &planner_types::ir::schema::SummaryUpdate::column(
            planner_types::ir::scalar::ColumnRef::SampleValue,
        ),
        &Default::default(),
    )
    .map_err(Error::Invalid)
}

/// A sketch evaluation is native only for the families Planner can read directly.
pub fn validate_sketch_evaluation(
    family: &SummaryFamilyType,
    query: &SketchStatistic,
) -> Result<(), Error> {
    validate_native_family(family)?;
    use planner_types::ir::schema::SketchAlgorithm as A;
    // A point count without an item value reads the total count.
    let bare_count = matches!(query, SketchStatistic::PointCount { value: None, .. });
    let supported = match family {
        SummaryFamilyType::Sketch(kind, _) => match (kind.algorithm(), query) {
            (A::Kll, SketchStatistic::Quantile { q })
            | (A::DDSketch, SketchStatistic::Quantile { q }) => {
                if !(0.0..=1.0).contains(q) {
                    return Err(Error::Invalid(
                        "quantile evaluation requires quantile in [0,1]".into(),
                    ));
                }
                true
            }
            (A::DDSketch, _) => bare_count,
            (A::Hll, SketchStatistic::Cardinality) => true,
            (A::Hll, _) => bare_count,
            (A::UnivMon, SketchStatistic::PointCount { value: None, .. })
            | (A::UnivMon, SketchStatistic::Cardinality)
            | (A::UnivMon, SketchStatistic::FrequencyL2)
            | (A::UnivMon, SketchStatistic::FrequencyEntropy) => true,
            // HydraCms answers a group's item frequency as well as its total.
            (A::Cms, SketchStatistic::PointCount { .. })
                if matches!(family, SummaryFamilyType::Sketch(_, grouping)
                    if grouping != &GroupingStrategy::PerSubpopulationInstance) =>
            {
                true
            }
            // Only count intents read a Count-Min bare count, and their
            // updates have unit weight; the evaluation is typed Int64 on that basis.
            (A::Cms, _) => bare_count,
            _ => false,
        },
        _ => false,
    };
    if !supported {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    Ok(())
}

/// An exact evaluation must match the exact family it reads.
pub fn validate_exact_evaluation(
    family: &SummaryFamilyType,
    evaluation: &crate::summary_kernels::exact::ExactEvaluation,
) -> Result<(), Error> {
    validate_native_family(family)?;
    use crate::Statistic as S;
    use planner_types::ir::schema::ExactKind as E;
    let supported = matches!(
        (family, evaluation.statistic),
        (SummaryFamilyType::ExactAggregate(E::Sum, _), S::Sum)
            | (SummaryFamilyType::ExactAggregate(E::Count, _), S::Count)
            | (SummaryFamilyType::ExactAggregate(E::Min, _), S::Min)
            | (SummaryFamilyType::ExactAggregate(E::Max, _), S::Max)
            | (SummaryFamilyType::ExactAggregate(E::Rate, _), S::Rate)
            | (
                SummaryFamilyType::ExactAggregate(E::Increase, _),
                S::Increase
            )
    );
    if !supported {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    if evaluation.lookback_ms.is_some_and(|lookback| {
        lookback <= 0 || !matches!(evaluation.statistic, S::Rate | S::Increase)
    }) {
        return Err(Error::Invalid("invalid exact counter lookback".into()));
    }
    Ok(())
}

/// This executor's capabilities, as the planner's deployment input
/// ([`DeploymentCapabilities`]): every summary family and layout whose native
/// state this module accepts, with the readouts it evaluates. Parameters are
/// representative; sizing limits are checked when a plan is bound. The
/// executor maintains state at ingestion time, keeps no query-time result
/// across evaluations, and reads raw data from its inputs, so raw retention
/// is the deployment's and is not priced.
pub fn capabilities() -> planner_types::deployment::DeploymentCapabilities {
    use planner_types::deployment::{summary_of, DeploymentCapabilities, Readout, SummarySupport};
    use planner_types::ir::scalar::ColumnRef;
    use planner_types::ir::schema::{default_hydra_params, HydraKind, SketchKind};
    let exact = [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Count, ExactParams::Count),
        (ExactKind::Min, ExactParams::Min),
        (ExactKind::Max, ExactParams::Max),
        (ExactKind::Increase, ExactParams::Increase),
        (ExactKind::Rate, ExactParams::Rate),
        (ExactKind::IRate, ExactParams::IRate),
    ]
    .map(|(kind, params)| SummaryFamilyType::ExactAggregate(kind, params));
    let (width, depth, heap_size) = (1024, 5, 16);
    let sketches = [
        (SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        (
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
        ),
        (SketchAlgorithm::Hll, SketchParams::Hll { precision: 14 }),
        (SketchAlgorithm::Cms, SketchParams::Cms { width, depth }),
        (
            SketchAlgorithm::CountSketch,
            SketchParams::CountSketch { width, depth },
        ),
        (
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            },
        ),
        (
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            },
        ),
        (
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size,
                sketch_rows: depth,
                sketch_cols: width,
                layers: 8,
            },
        ),
        (SketchAlgorithm::Kmv, SketchParams::Kmv { k: 1024 }),
        (SketchAlgorithm::Theta, SketchParams::Theta { k: 1024 }),
    ];
    let hydra = [
        HydraKind::HydraCms,
        HydraKind::HydraCountSketch,
        HydraKind::HydraKll,
    ];
    let sketches = sketches.into_iter().flat_map(|(algorithm, params)| {
        let layouts = std::iter::once(GroupingStrategy::PerSubpopulationInstance).chain(
            hydra.iter().filter_map(|kind| {
                default_hydra_params(kind.clone(), &params).map(|params| {
                    GroupingStrategy::SharedMultiSubpopulation {
                        kind: kind.clone(),
                        params,
                    }
                })
            }),
        );
        let kind = SketchKind::new(algorithm, params.clone());
        layouts
            .map(|layout| SummaryFamilyType::Sketch(kind.clone(), layout))
            .collect::<Vec<_>>()
    });
    let item = |value: Option<&str>| SketchStatistic::PointCount {
        key: match value {
            None => ColumnRef::SampleValue,
            Some(_) => ColumnRef::Named("item".into()),
        },
        value: value.map(Into::into),
    };
    let readouts = [
        SketchStatistic::Quantile { q: 0.5 },
        item(None),
        item(Some("item")),
        SketchStatistic::Cardinality,
        SketchStatistic::TopK { k: 1 },
        SketchStatistic::FrequencyL2,
        SketchStatistic::FrequencyEntropy,
    ];
    let summaries = exact
        .into_iter()
        .chain(sketches)
        .filter(builds_from_rows)
        .map(|family| {
            let (summary, layout) = summary_of(&family).expect("a summary family");
            let readouts = match &family {
                SummaryFamilyType::Sketch(kind, _) => readouts
                    .iter()
                    .filter(|statistic| match statistic {
                        // Read by the keyed evaluation operator.
                        SketchStatistic::TopK { .. } => {
                            crate::summary_kernels::weighted_frequency::WeightedFrequency::configuration(kind)
                                .is_ok()
                        }
                        _ => validate_sketch_evaluation(&family, statistic).is_ok(),
                    })
                    .map(Readout::of)
                    .collect(),
                _ => Default::default(),
            };
            SummarySupport {
                family: summary,
                layout,
                readouts,
            }
        })
        .collect();
    DeploymentCapabilities {
        summaries: Some(summaries),
        ingestion_time: true,
        query_time_retention: false,
        ..DeploymentCapabilities::UNRESTRICTED
    }
}

/// Whether a plan can build `family` from rows: its native state is
/// accepted, except a per-group Count-Min, which is native as stored state
/// only (see [`validate_native_family`]).
fn builds_from_rows(family: &SummaryFamilyType) -> bool {
    let per_group_cms = matches!(family, SummaryFamilyType::Sketch(kind, grouping)
        if kind.algorithm() == &SketchAlgorithm::Cms
            && grouping == &GroupingStrategy::PerSubpopulationInstance);
    !per_group_cms && validate_native_family(family).is_ok()
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;
    use planner_types::deployment::{InstanceLayout, Readout, SummaryFamily};
    use planner_types::ir::schema::HydraKind;

    fn readouts(
        caps: &planner_types::deployment::DeploymentCapabilities,
        family: SummaryFamily,
        layout: InstanceLayout,
    ) -> Option<Vec<Readout>> {
        caps.summaries
            .as_ref()
            .unwrap()
            .iter()
            .find(|s| s.family == family && s.layout == layout)
            .map(|s| s.readouts.iter().copied().collect())
    }

    /// The exported set follows this module's rules: e.g. KLL quantiles,
    /// heap top-k, HydraCms counts, exact accumulators without IRate, and
    /// no per-group Count-Min build.
    #[test]
    fn exported_capabilities_follow_the_validators() {
        use InstanceLayout::{Hydra, PerGroup};
        use SummaryFamily::{Exact, Sketch};
        let caps = capabilities();
        assert!(caps.ingestion_time && !caps.query_time_retention);
        assert!(caps.raw_data_retained && caps.memory_budget_bytes.is_none());
        let of = |family, layout| readouts(&caps, family, layout);
        assert_eq!(
            of(Sketch(SketchAlgorithm::Kll), PerGroup),
            Some(vec![Readout::Quantile])
        );
        for heap in [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ] {
            assert_eq!(of(Sketch(heap), PerGroup), Some(vec![Readout::TopK]));
        }
        assert_eq!(
            of(Sketch(SketchAlgorithm::Cms), Hydra(HydraKind::HydraCms)),
            Some(vec![Readout::TotalCount, Readout::ItemCount])
        );
        assert_eq!(of(Sketch(SketchAlgorithm::Cms), PerGroup), None);
        assert_eq!(
            of(Sketch(SketchAlgorithm::Kll), Hydra(HydraKind::HydraKll)),
            None
        );
        assert_eq!(of(Sketch(SketchAlgorithm::CountSketch), PerGroup), None);
        assert_eq!(of(Exact(ExactKind::Rate), PerGroup), Some(vec![]));
        assert_eq!(of(Exact(ExactKind::IRate), PerGroup), None);
    }
}
