//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr,
};
use asap_types::summary_type::{
    ExactKind, ExactParams, MergeInputRule, MergedError, SamplingKind, SamplingParams,
    SketchAlgorithm, SketchStatistic, StatModelKind, StatModelParams, StatisticKind,
    SummaryTypeCapability, SummaryUpdate, WaveletKind, WaveletParams,
};
use std::rc::Rc;
/// KLL over `value` for one `region`, so states of different regions are
/// disjoint.
fn state(_k: u32, region: &str) -> Rc<OperatorNode> {
    family_state(FieldDataType::Sketch(SketchAlgorithm::Kll), region)
}
/// A `family` state over `value` for one `region`.
fn family_state(family: FieldDataType, region: &str) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "latencies".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain("region", DataType::Utf8, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    }))
    .unwrap();
    let only_region = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(region.into()))),
            semantics: ExprSemantics::Sql,
        }),
        child: scan,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: only_region,
        summary_type: family,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        filter: None,
    }))
    .unwrap()
}
/// Two KLL tumbling windows compose into one typed logical state without timing assignment.
#[test]
fn compatible_tumbling_windows_merge_structurally() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200, "us"), state(200, "eu")],
        group_by: None,
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
}
/// An empty merge and raw rows cannot masquerade as compatible tumbling
/// windows. (State sizes are no longer part of the state's type: θ is a
/// candidate's `LogicalProperties::parameters`.)
#[test]
fn incompatible_merge_inputs_fail() {
    for children in [vec![], vec![state(200, "us").children()[0].clone()]] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }))
            .is_err()
        );
    }
}

fn sketch(algorithm: SketchAlgorithm) -> FieldDataType {
    FieldDataType::Sketch(algorithm)
}
fn exact(kind: ExactKind, params: ExactParams) -> FieldDataType {
    FieldDataType::ExactAggregate(kind, params)
}
fn kll() -> FieldDataType {
    sketch(SketchAlgorithm::Kll)
}
fn hll() -> FieldDataType {
    sketch(SketchAlgorithm::Hll)
}
fn cms_with_heap() -> FieldDataType {
    sketch(SketchAlgorithm::CmsWithHeap)
}
fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, String> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        group_by: None,
    }))
    .map_err(|e| e.to_string())
}

/// The summary-type capability table, row by row: how each type's merge
/// inputs must relate, and which statistics one state answers with the
/// error of each from a merged state. Every statistic kind is probed
/// through `answers` and `merged_error`, so the table and both lookups are
/// pinned together.
#[test]
fn summary_type_capability_table() {
    use MergeInputRule::{Disjoint, DisjointInTime, MayOverlap};
    // (family, how its merge inputs must relate, its readouts).
    type Row = (
        FieldDataType,
        Option<MergeInputRule>,
        Vec<(StatisticKind, MergedError)>,
    );
    let rows: Vec<Row> = vec![
        (
            exact(ExactKind::Sum, ExactParams::Sum),
            Some(Disjoint),
            vec![],
        ),
        (
            exact(ExactKind::Count, ExactParams::Count),
            Some(Disjoint),
            vec![],
        ),
        (
            exact(ExactKind::Min, ExactParams::Min),
            Some(MayOverlap),
            vec![],
        ),
        (
            exact(ExactKind::Max, ExactParams::Max),
            Some(MayOverlap),
            vec![],
        ),
        // Counter tumbling-window merge exactly when ordered and disjoint in time.
        (
            exact(ExactKind::Rate, ExactParams::Rate),
            Some(DisjointInTime),
            vec![],
        ),
        (
            exact(ExactKind::Increase, ExactParams::Increase),
            Some(DisjointInTime),
            vec![],
        ),
        (exact(ExactKind::IRate, ExactParams::IRate), None, vec![]),
        (FieldDataType::Plain(DataType::Float64), None, vec![]),
        (
            FieldDataType::SummarySample(
                SamplingKind::Reservoir,
                SamplingParams::Reservoir { size: 64 },
            ),
            None,
            vec![],
        ),
        (
            FieldDataType::Wavelet(WaveletKind::Haar, WaveletParams::Haar { coefficients: 16 }),
            None,
            vec![],
        ),
        (
            FieldDataType::StatModel(
                StatModelKind::Parametric,
                StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            ),
            None,
            vec![],
        ),
    ];
    // A sketch family's row is asap_sketchlib's, unchanged; a Hydra reads
    // the row of its Hydra family.
    let lib = |algorithm: SketchAlgorithm| {
        let row = algorithm.capability();
        (
            row.merge_inputs.map(MergeInputRule::from),
            row.readouts.to_vec(),
        )
    };
    let sketches = [
        (kll(), SketchAlgorithm::Kll),
        (sketch(SketchAlgorithm::DDSketch), SketchAlgorithm::DDSketch),
        (hll(), SketchAlgorithm::Hll),
        (sketch(SketchAlgorithm::Cms), SketchAlgorithm::Cms),
        (
            sketch(SketchAlgorithm::CountSketch),
            SketchAlgorithm::CountSketch,
        ),
        (sketch(SketchAlgorithm::UnivMon), SketchAlgorithm::UnivMon),
        (sketch(SketchAlgorithm::Kmv), SketchAlgorithm::Kmv),
        (cms_with_heap(), SketchAlgorithm::CmsWithHeap),
        (
            sketch(SketchAlgorithm::CountSketchWithHeap),
            SketchAlgorithm::CountSketchWithHeap,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::HydraCms),
            SketchAlgorithm::HydraCms,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::HydraCountSketch),
            SketchAlgorithm::HydraCountSketch,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::HydraKll),
            SketchAlgorithm::HydraKll,
        ),
    ];
    let rows = rows
        .into_iter()
        .chain(sketches.into_iter().map(|(family, algorithm)| {
            let (merge_inputs, readouts) = lib(algorithm);
            (family, merge_inputs, readouts)
        }));
    // One statistic of every kind.
    let statistics = [
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        SketchStatistic::PointCount {
            key: ColumnRef::Named("item".into()),
            value: Some("a".into()),
        },
        SketchStatistic::Quantile { q: 0.99 },
        SketchStatistic::Cardinality,
        SketchStatistic::FrequencyL2,
        SketchStatistic::FrequencyEntropy,
        SketchStatistic::TopK { k: 5 },
    ];
    for (family, merge_inputs, readouts) in rows {
        let capability = SummaryTypeCapability::of(&family);
        assert_eq!(capability.merge_inputs, merge_inputs, "{family:?}");
        assert_eq!(capability.readouts().to_vec(), readouts, "{family:?}");
        for statistic in &statistics {
            let row = readouts
                .iter()
                .find(|(kind, _)| *kind == statistic.kind())
                .map(|&(_, error)| error);
            assert_eq!(
                capability.answers(statistic),
                row.is_some(),
                "{family:?} {statistic:?}"
            );
            // A merged state's error is its row's; none for a statistic
            // the family does not answer.
            assert_eq!(
                capability.merged_error(statistic),
                merge_inputs.and(row),
                "{family:?} {statistic:?}"
            );
        }
    }
}

/// Heap readouts (entropy, top-k) of a merged state are approximate: each
/// part's heaps keep only that part's heavy items. UnivMon's total adds
/// exactly. Its L2 reads layer 0's Count Sketch, which adds, and its
/// distinct count's bound holds for any heap that keeps `min(|S_j|, k)` of
/// a layer's keys, as the merge's does, so both keep the guarantee of one
/// build over the union. A statistic the family does not answer has no
/// merged error.
#[test]
fn merged_error_depends_on_the_readout() {
    use SketchStatistic::{Cardinality, FrequencyEntropy, FrequencyL2, TopK};
    let point = || SketchStatistic::PointCount {
        key: ColumnRef::Named("k".into()),
        value: None,
    };
    let univmon = SummaryTypeCapability::of(&sketch(SketchAlgorithm::UnivMon));
    for (readout, error) in [
        (FrequencyL2, MergedError::AsIfBuiltOverUnion),
        (point(), MergedError::Exact),
        (Cardinality, MergedError::AsIfBuiltOverUnion),
        (FrequencyEntropy, MergedError::Approximate),
    ] {
        assert_eq!(univmon.merged_error(&readout), Some(error), "{readout:?}");
    }
    assert_eq!(univmon.merged_error(&TopK { k: 5 }), None);
    let heap = SummaryTypeCapability::of(&cms_with_heap());
    assert_eq!(
        heap.merged_error(&TopK { k: 5 }),
        Some(MergedError::Approximate)
    );
    assert_eq!(heap.merged_error(&point()), None);
    // Count Sketch's signed counters give no total: no merged error for a
    // bare point count, and one build's guarantee for an item's frequency.
    let count_sketch = SummaryTypeCapability::of(&sketch(SketchAlgorithm::CountSketch));
    assert!(!count_sketch.answers(&point()));
    assert_eq!(count_sketch.merged_error(&point()), None);
    let item = SketchStatistic::PointCount {
        key: ColumnRef::Named("k".into()),
        value: Some("a".into()),
    };
    assert_eq!(
        count_sketch.merged_error(&item),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    // An exact accumulator is read as its finalized value, not a readout.
    let sum = exact(ExactKind::Sum, ExactParams::Sum);
    assert_eq!(SummaryTypeCapability::of(&sum).merged_error(&point()), None);
    assert_eq!(
        SummaryTypeCapability::of(&hll()).merged_error(&Cardinality),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    assert_eq!(
        SummaryTypeCapability::of(&kll()).merged_error(&SketchStatistic::Quantile { q: 0.99 }),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    let irate = exact(ExactKind::IRate, ExactParams::IRate);
    assert_eq!(
        SummaryTypeCapability::of(&irate).merged_error(&point()),
        None
    );
}

/// A structurally validated `SummaryEstimate` of `query` over a `family`
/// state.
fn estimate(family: FieldDataType, query: SketchStatistic) -> Result<Rc<OperatorNode>, String> {
    let node = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: family_state(family, "us"),
        query,
    }))
    .map_err(|e| e.to_string())?;
    node.validate_structure().map_err(|e| e.to_string())?;
    Ok(node)
}

/// A statistic the family does not answer is rejected: a DDSketch's or an
/// HLL's total, a quantile over a Count-Min, top-k over a KLL, and any
/// readout of an exact accumulator. What the table answers builds.
#[test]
fn a_statistic_the_family_does_not_answer_is_rejected() {
    let cms = sketch(SketchAlgorithm::Cms);
    let ddsketch = sketch(SketchAlgorithm::DDSketch);
    let total = || SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    };
    for (family, query) in [
        (ddsketch.clone(), total()),
        (hll(), total()),
        (cms.clone(), SketchStatistic::Quantile { q: 0.9 }),
        (kll(), SketchStatistic::TopK { k: 5 }),
        (exact(ExactKind::Count, ExactParams::Count), total()),
    ] {
        let error = estimate(family.clone(), query.clone()).unwrap_err();
        assert!(
            error.contains("evaluation does not match its summary type"),
            "{family:?} {query:?}: {error}"
        );
    }
    estimate(cms, total()).unwrap();
    estimate(ddsketch, SketchStatistic::Quantile { q: 0.9 }).unwrap();
    estimate(hll(), SketchStatistic::Cardinality).unwrap();
}

/// What the `SummaryEstimate` validation accepted before it read the
/// capability table: a match on the algorithm's category, a Hydra
/// algorithm classified by the sketch in its cells.
fn accepted_by_category(algorithm: &SketchAlgorithm, query: &SketchStatistic) -> bool {
    use SketchAlgorithm as A;
    use SketchStatistic as Q;
    match algorithm {
        A::Kll | A::DDSketch => matches!(query, Q::Quantile { .. }),
        A::Hll | A::Kmv => matches!(query, Q::Cardinality),
        A::Cms | A::CountSketch => matches!(query, Q::PointCount { .. }),
        A::CmsWithHeap | A::CountSketchWithHeap => matches!(query, Q::TopK { .. }),
        A::UnivMon => !matches!(query, Q::Quantile { .. }),
        A::HydraKll => matches!(query, Q::Quantile { .. }),
        A::HydraCms | A::HydraCountSketch => matches!(query, Q::PointCount { .. }),
        _ => unreachable!("{algorithm:?} is not probed"),
    }
}

/// Reading the family-capability table changes what the `SummaryEstimate`
/// validation accepts only where the old category match was wrong: a
/// UnivMon answers no item frequency or top-k, a Count Sketch's signed
/// counters give no total, and a heap family's counters answer an item's
/// frequency. Every other (family, statistic) pair keeps its
/// answer.
#[test]
fn estimate_validation_matches_the_category_match_except_the_fixed_rows() {
    use SketchAlgorithm as A;
    let mut families: Vec<FieldDataType> = [
        A::Kll,
        A::DDSketch,
        A::Hll,
        A::Kmv,
        A::Cms,
        A::CountSketch,
        A::CmsWithHeap,
        A::CountSketchWithHeap,
        A::UnivMon,
    ]
    .into_iter()
    .map(sketch)
    .collect();
    families.extend([
        FieldDataType::Sketch(SketchAlgorithm::HydraCms),
        FieldDataType::Sketch(SketchAlgorithm::HydraCountSketch),
        FieldDataType::Sketch(SketchAlgorithm::HydraKll),
    ]);
    let statistics = [
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        SketchStatistic::PointCount {
            key: ColumnRef::Named("item".into()),
            value: Some("a".into()),
        },
        SketchStatistic::Quantile { q: 0.99 },
        SketchStatistic::Cardinality,
        SketchStatistic::FrequencyL2,
        SketchStatistic::FrequencyEntropy,
        SketchStatistic::TopK { k: 5 },
    ];
    for family in families {
        let FieldDataType::Sketch(kind) = &family else {
            unreachable!()
        };
        for query in &statistics {
            let fixed = matches!(
                (kind, query.kind()),
                (
                    SketchAlgorithm::UnivMon,
                    StatisticKind::ItemFrequency | StatisticKind::TopK
                ) | (
                    SketchAlgorithm::CountSketch | SketchAlgorithm::HydraCountSketch,
                    StatisticKind::Total
                ) | (
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap,
                    StatisticKind::ItemFrequency,
                )
            );
            let before = accepted_by_category(kind, query);
            let after = estimate(family.clone(), query.clone()).is_ok();
            assert_eq!(after, before != fixed, "{family:?} {query:?}");
        }
    }
}

/// The IR does not reject a merge by family: exact IRate has no sound
/// merge, but only the table's consumers (the planner rules and the
/// executor) decide that.
#[test]
fn summary_merge_is_permissive_about_the_family() {
    let irate = || exact(ExactKind::IRate, ExactParams::IRate);
    let merged = merge(vec![
        family_state(irate(), "us"),
        family_state(irate(), "eu"),
    ])
    .unwrap();
    merged.validate_structure().unwrap();
    assert_eq!(SummaryTypeCapability::of(&irate()).merge_inputs, None);
}
