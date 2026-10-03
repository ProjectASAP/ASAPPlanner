//! Coverage composition preserves gaps and rejects duplicate observations.
use asap_types::{
    ir::operator_properties::Reduction, ir::summary_coverage::*, post_asap::SummaryUpdate,
    pre_asap::ColumnRef,
};
fn coverage(start: i64, end: i64, population: &[(&str, &str)]) -> SummaryCoverage {
    SummaryCoverage {
        source: "flows".into(),
        revision: "snapshot-1".into(),
        input: SummaryUpdate::column(ColumnRef::Named("latency".into())),
        grouping: Reduction::by(vec![0]),
        multiplicity: ObservationMultiplicity::OncePerObservation,
        regions: vec![CoverageRegion {
            start_ms: start,
            end_ms: end,
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
    assert_eq!(merged.regions[0].end_ms, 2);
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
    let mut other = coverage(1, 2, &[]);
    other.revision = "snapshot-2".into();
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), other]),
        Err(CoverageError::IncompatibleInput)
    );
    assert_eq!(
        coverage(2, 1, &[]).validate(),
        Err(CoverageError::InvalidInterval)
    );
}

/// Coverage is logical state metadata, and input rewrites invalidate its proof.
#[test]
fn node_coverage_is_checked_and_rewrites_clear_it() {
    use asap_types::{
        ir::operator_properties::Source,
        ir::{ASAPOp, NonASAPOp, Operator, OperatorNode},
        post_asap::{SketchAlgorithm, SketchKind, SketchParams},
        pre_asap::{DataType, Field, FieldDataType, Schema},
    };
    let raw = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "flows".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("latency", DataType::Float64, false)]),
    }))
    .unwrap();
    let mut declared = coverage(0, 1, &[]);
    declared.grouping = Reduction::by(vec![]);
    assert!((*raw)
        .clone()
        .with_summary_coverage(declared.clone())
        .is_err());
    let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: raw,
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: declared.input.clone(),
        reduction: declared.grouping.clone(),
        grouping: Default::default(),
        filter: None,
    }))
    .unwrap();
    let state = state.with_summary_coverage(declared.clone()).unwrap();
    assert!(state.summary_coverage.is_some());
    let mut bad = declared;
    bad.input = SummaryUpdate::column(ColumnRef::Named("other".into()));
    assert!(state.clone().with_summary_coverage(bad).is_err());
    let rebuilt = state.map_children(Clone::clone).unwrap();
    assert!(rebuilt.summary_coverage.is_none());
}
