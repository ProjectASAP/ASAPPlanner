//! Coverage composition preserves gaps and rejects duplicate observations.
use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::operator::Source;
use asap_types::ir::properties::summary_coverage::*;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::SummaryUpdate;
fn table(name: &str) -> Source {
    Source::Table {
        table_ref: name.into(),
    }
}
fn coverage(start: i64, end: i64, population: &[(&str, &str)]) -> SummaryCoverage {
    SummaryCoverage {
        source: table("flows"),
        regions: vec![CoverageRegion {
            time_ms: Some((start..end).into()),
            population: population
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }],
    }
}
/// Adjacent panes coalesce; gaps remain disconnected rather than becoming a hull.
#[test]
fn time_union_preserves_gaps() {
    let merged =
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), coverage(1, 2, &[])]).unwrap();
    assert_eq!(merged.regions[0].time_ms, Some((0..2).into()));
    assert_eq!(merged.regions.len(), 1);
    let gapped =
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), coverage(2, 3, &[])]).unwrap();
    assert_eq!(gapped.regions.len(), 2);
}
/// Population partitions can overlap in time without sharing observations.
#[test]
fn population_and_joint_union() {
    let merged = SummaryCoverage::merge_disjoint(&[
        coverage(0, 2, &[("region", "us")]),
        coverage(0, 2, &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(merged.regions.len(), 2);
    let joint = SummaryCoverage::merge_disjoint(&[
        coverage(0, 1, &[("region", "us")]),
        coverage(1, 2, &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(joint.regions.len(), 2);
    let decoded: SummaryCoverage =
        serde_json::from_str(&serde_json::to_string(&joint).unwrap()).unwrap();
    assert_eq!(decoded, joint);
}
/// Intersecting predicates and windows cannot authorize once-per-observation merge.
#[test]
fn overlap_and_identity_fail_closed() {
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[coverage(0, 2, &[]), coverage(1, 3, &[])]),
        Err(CoverageError::PossibleOverlap)
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[
            coverage(0, 2, &[("region", "us")]),
            coverage(0, 2, &[("tier", "premium")])
        ]),
        Err(CoverageError::PossibleOverlap)
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[
            coverage(0, 2, &[("region", "us")]),
            coverage(0, 2, &[("region", "us")])
        ]),
        Err(CoverageError::PossibleOverlap)
    );
    let mut other = coverage(1, 2, &[]);
    other.source = table("other-flows");
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), other]),
        Err(CoverageError::SourceMismatch)
    );
    assert_eq!(
        coverage(2, 1, &[]).validate(),
        Err(CoverageError::InvalidInterval)
    );
}

/// Coverage is logical state metadata, and input rewrites invalidate its proof.
#[test]
fn node_coverage_is_required_checked_and_cleared_by_rewrites() {
    use asap_types::ir::schema::{
        DataType, Field, FieldDataType, Schema, SketchAlgorithm, SketchKind, SketchParams,
    };
    use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
    let raw = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: table("flows"),
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("latency", DataType::Float64, false)]),
    }))
    .unwrap();
    let declared = coverage(0, 1, &[]);
    assert!((*raw).clone().with_coverage(declared.clone()).is_err());
    let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: raw,
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("latency".into())),
        reduction: Reduction::by(vec![]),
        grouping: Default::default(),
        filter: None,
    }))
    .unwrap();
    // Summary nodes cannot validate without coverage.
    assert!(matches!(
        std::rc::Rc::new(state.clone()).validate_structure(),
        Err(asap_types::ir::SchemaDerivationError::Coverage(
            CoverageError::Missing
        ))
    ));
    let state = state.with_coverage(declared.clone()).unwrap();
    std::rc::Rc::new(state.clone())
        .validate_structure()
        .unwrap();
    let rebuilt = state.map_children(Clone::clone).unwrap();
    assert!(rebuilt.coverage.is_none());
}

/// Sources without a time column declare no time bounds; such a region overlaps
/// any region it is not population-disjoint from.
#[test]
fn regions_without_time_bounds() {
    let mut tabular = coverage(0, 1, &[("region", "us")]);
    tabular.regions[0].time_ms = None;
    let mut other = coverage(0, 1, &[("region", "eu")]);
    other.regions[0].time_ms = None;
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[tabular.clone(), other])
            .unwrap()
            .regions
            .len(),
        2
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[tabular, coverage(5, 6, &[("region", "us")])]),
        Err(CoverageError::PossibleOverlap)
    );
}

fn relative(start: i64, end: i64) -> SummaryCoverage {
    let mut coverage = coverage(0, 1, &[]);
    coverage.regions[0].time_ms = Some(CoverageTime::RelativeToEvaluation(start..end));
    coverage
}
const MINUTE: i64 = 60_000;
/// Pane i of width w covers `[-(i+1)w, -iw)`; five adjacent 1 m panes
/// coalesce into the 5 m window ending at evaluation time.
#[test]
fn adjacent_relative_panes_coalesce() {
    let panes: Vec<_> = (0..5)
        .map(|i| relative(-(i + 1) * MINUTE, -i * MINUTE))
        .collect();
    let merged = SummaryCoverage::merge_disjoint(&panes).unwrap();
    assert_eq!(
        merged.regions[0].time_ms,
        Some(CoverageTime::RelativeToEvaluation(-5 * MINUTE..0))
    );
    assert_eq!(merged.regions.len(), 1);
}
/// A missing relative pane leaves a gap instead of a hull.
#[test]
fn relative_gap_is_kept() {
    let merged = SummaryCoverage::merge_disjoint(&[
        relative(-3 * MINUTE, -2 * MINUTE),
        relative(-MINUTE, 0),
    ])
    .unwrap();
    assert_eq!(merged.regions.len(), 2);
}
/// Overlapping relative panes would count observations twice.
#[test]
fn overlapping_relative_regions_are_rejected() {
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[relative(-2 * MINUTE, 0), relative(-MINUTE, 0)]),
        Err(CoverageError::PossibleOverlap)
    );
}
/// Relative and absolute time are incomparable without an evaluation time, so
/// even numerically disjoint ranges cannot be proven disjoint.
#[test]
fn mixing_relative_and_absolute_time_is_rejected() {
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[relative(-MINUTE, 0), coverage(0, MINUTE, &[])]),
        Err(CoverageError::MixedTimeAnchors)
    );
    let mut mixed = relative(-MINUTE, 0);
    mixed.regions.extend(coverage(0, MINUTE, &[]).regions);
    assert_eq!(mixed.validate(), Err(CoverageError::MixedTimeAnchors));
}
/// Relative time round-trips through serde, and a legacy plain `time_ms`
/// range still deserializes as absolute.
#[test]
fn time_anchor_serde_round_trip_and_legacy_json() {
    let pane = relative(-MINUTE, 0);
    let json = serde_json::to_value(&pane).unwrap();
    assert_eq!(
        json["regions"][0]["time_ms"],
        serde_json::json!({"relative_to_evaluation": {"start": -MINUTE, "end": 0}})
    );
    assert_eq!(
        serde_json::from_value::<SummaryCoverage>(json).unwrap(),
        pane
    );
    let absolute = coverage(0, MINUTE, &[]);
    let json = serde_json::to_value(&absolute).unwrap();
    assert_eq!(
        json["regions"][0]["time_ms"],
        serde_json::json!({"start": 0, "end": MINUTE})
    );
    let legacy = r#"{"source":{"Table":{"table_ref":"flows"}},
        "regions":[{"time_ms":{"start":0,"end":60000},"population":{}}]}"#;
    assert_eq!(
        serde_json::from_str::<SummaryCoverage>(legacy).unwrap(),
        absolute
    );
}
