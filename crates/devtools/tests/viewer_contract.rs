//! `tools/dag-viewer` ↔ IR contract: the viewer's `KIND_CATEGORY_JSON` must
//! categorize exactly the `Operator::kind_name()` strings of every
//! `NonASAPOp` and `ASAPOp` variant — no more (a stale kind the IR no longer
//! has) and no less (a kind the viewer would render uncategorized).

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::maintained_population::{
    MaintainedPopulation, PopulationInput, PopulationStatistic,
};
use asap_types::ir::operator::{
    BinaryOpKind, GroupKeys, JoinKind, Reduction, RelationalSetOpKind, SampleKind, Source,
    TimeShift, WindowFuncKind,
};
use asap_types::ir::scalar::{ArithmeticOpKind, ColumnRef, ScalarValue};
use asap_types::ir::schema::sketch::{GroupingStrategy, SketchStatistic, SummaryUpdate};
use asap_types::ir::schema::{DataType, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr, TimeRangeKind,
};

/// One instance of every `NonASAPOp` variant, children elided. The
/// exhaustive match fails to compile when a variant is added, until a sample
/// for it is added here too.
fn non_asap_samples() -> Vec<NonASAPOp<()>> {
    let pred = || Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)));
    let samples = vec![
        NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::default(),
        },
        NonASAPOp::Values {
            rows: vec![],
            schema: Schema::default(),
        },
        NonASAPOp::Filter {
            pred: pred(),
            child: (),
        },
        NonASAPOp::Project {
            cols: vec![],
            qualifier: None,
            child: (),
        },
        NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: (),
        },
        NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: pred(),
            left: (),
            right: (),
        },
        NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            all: true,
            left: (),
            right: (),
        },
        NonASAPOp::Concat {
            children: vec![],
            discriminator_unique_key: None,
        },
        NonASAPOp::Dedup {
            cols: vec![],
            child: (),
        },
        NonASAPOp::Sort {
            keys: vec![],
            partition_by: GroupKeys::default(),
            child: (),
        },
        NonASAPOp::Limit {
            n: None,
            offset: 0,
            partition_by: GroupKeys::default(),
            child: (),
        },
        NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                vector_match: None,
            },
            return_bool: false,
            lhs: (),
            rhs: (),
        },
        NonASAPOp::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::default(),
            order_by: vec![],
            frame: None,
            output_name: "w".into(),
            child: (),
        },
        NonASAPOp::TimeRange {
            range: Duration::from_secs(60),
            kind: TimeRangeKind::Range,
            child: (),
        },
        NonASAPOp::TimeShift {
            shift: TimeShift::default(),
            child: (),
        },
        NonASAPOp::PromqlVectorFromScalar(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
        NonASAPOp::PromqlRelabel {
            dst: "l".into(),
            value: ScalarExpr::Literal(ScalarValue::Utf8("v".into())),
            child: (),
        },
        NonASAPOp::PromqlInfoEnrich {
            selector: vec![],
            child: (),
        },
        NonASAPOp::PromqlSeriesSample {
            by: GroupKeys::default(),
            kind: SampleKind::LimitK(1),
            child: (),
        },
        NonASAPOp::PromqlSubquery {
            range: Duration::from_secs(60),
            resolution: None,
            child: (),
        },
    ];
    for sample in &samples {
        match sample {
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::Aggregate { .. }
            | NonASAPOp::Join { .. }
            | NonASAPOp::SetOp { .. }
            | NonASAPOp::Concat { .. }
            | NonASAPOp::Dedup { .. }
            | NonASAPOp::Sort { .. }
            | NonASAPOp::Limit { .. }
            | NonASAPOp::BinaryOp { .. }
            | NonASAPOp::SQLWindowFunc { .. }
            | NonASAPOp::TimeRange { .. }
            | NonASAPOp::TimeShift { .. }
            | NonASAPOp::PromqlVectorFromScalar(_)
            | NonASAPOp::PromqlRelabel { .. }
            | NonASAPOp::PromqlInfoEnrich { .. }
            | NonASAPOp::PromqlSeriesSample { .. }
            | NonASAPOp::PromqlSubquery { .. } => {}
        }
    }
    samples
}

/// One instance of every `ASAPOp` variant, children elided; see
/// [`non_asap_samples`].
fn asap_samples() -> Vec<ASAPOp<()>> {
    let family = FieldDataType::Plain(DataType::Float64);
    let rows = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::default(),
    }))
    .unwrap();
    let samples = vec![
        ASAPOp::SummaryAgg {
            child: (),
            family: family.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![]),
            grouping: GroupingStrategy::default(),
            filter: None,
        },
        ASAPOp::SummaryEstimate {
            summary_input: (),
            query: SketchStatistic::FrequencyL2,
        },
        ASAPOp::FinalizeExactAccumulator { child: () },
        ASAPOp::MaintainPopulation {
            child: (),
            population: MaintainedPopulation {
                input: PopulationInput::Rows {
                    input: Rc::clone(&rows),
                    value_column: 0,
                    grouping: GroupKeys::default(),
                },
                max_k: 1,
                quantiles: false,
            },
        },
        ASAPOp::EvaluatePopulation {
            child: (),
            evaluation: PopulationStatistic::Sum,
        },
        ASAPOp::SummaryMerge {
            children: vec![],
            newest_panes: None,
            group_by: None,
        },
        ASAPOp::PaneChain {
            child: (),
            family: family.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![]),
            grouping: GroupingStrategy::default(),
            filter: None,
            pane_width_ms: 60_000,
            pane_count: 5,
        },
        ASAPOp::SummarySubtract {
            left: (),
            right: (),
        },
        ASAPOp::SummaryDelete {
            summary_input: (),
            key: 0,
        },
        ASAPOp::SummaryJoin {
            outer: (),
            inner: (),
            key: 0,
            family,
        },
        ASAPOp::Extension {
            child: (),
            name: "x".into(),
        },
    ];
    for sample in &samples {
        match sample {
            ASAPOp::SummaryAgg { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. }
            | ASAPOp::MaintainPopulation { .. }
            | ASAPOp::EvaluatePopulation { .. }
            | ASAPOp::SummaryMerge { .. }
            | ASAPOp::PaneChain { .. }
            | ASAPOp::SummarySubtract { .. }
            | ASAPOp::SummaryDelete { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::Extension { .. } => {}
        }
    }
    samples
}

/// The viewer's `kind -> category` table, parsed out of the JS source the
/// same way the viewer itself does (`JSON.parse(KIND_CATEGORY_JSON)`).
fn viewer_kind_categories() -> BTreeMap<String, String> {
    const START: &str = "const KIND_CATEGORY_JSON = `";
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/dag-viewer/node-style.js"
    ));
    let json = source
        .split_once(START)
        .expect("node-style.js must declare KIND_CATEGORY_JSON")
        .1
        .split_once("`;")
        .expect("KIND_CATEGORY_JSON must be a template literal")
        .0;
    serde_json::from_str(json).expect("KIND_CATEGORY_JSON must be valid JSON")
}

#[test]
fn viewer_categorizes_exactly_the_exported_node_kinds() {
    let non_asap = non_asap_samples();
    let asap = asap_samples();
    let expected: BTreeSet<&str> = non_asap
        .iter()
        .map(|op| Operator::NonASAP(op.clone()).kind_name())
        .chain(asap.iter().map(|op| Operator::ASAP(op.clone()).kind_name()))
        .collect();
    assert_eq!(
        expected.len(),
        non_asap.len() + asap.len(),
        "exported kind names must be unique"
    );
    let categories = viewer_kind_categories();
    let actual: BTreeSet<&str> = categories.keys().map(String::as_str).collect();

    assert_eq!(actual, expected);
}

/// A JS array of strings declared as `const NAME = [...]` (or
/// `NAME.concat([...])`) in stages.js, read as the viewer reads it.
fn stages_js_strings(declaration: &str) -> BTreeSet<String> {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/dag-viewer/stages.js"
    ));
    let body = source
        .split_once(declaration)
        .unwrap_or_else(|| panic!("stages.js must declare {declaration}"))
        .1
        // The array's own `]`: its strings hold none, and a `concat([...])`
        // ends with `]);`, not `];`.
        .split_once(']')
        .expect("the array must end with `]`")
        .0;
    body.split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The serde name of a unit enum variant.
fn wire_name<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// The viewer knows every `MaterializedViewRefresh`
/// (`materialized_views[].refresh`) and every Stage 3 `RejectionCause`, and
/// splits the causes into failed checks and losses as
/// `RejectionCause::failed_a_check` does.
#[test]
fn viewer_knows_every_view_refresh_and_rejection_cause() {
    use asap_plan_selection::RejectionCause as C;
    use asap_planner::MaterializedViewRefresh as R;
    let refreshes = [R::AtIngestion, R::OnFirstRead];
    for refresh in refreshes {
        // A new variant fails to compile here until it is listed above.
        match refresh {
            R::AtIngestion | R::OnFirstRead => {}
        }
    }
    assert_eq!(
        stages_js_strings("const VIEW_REFRESHES = ["),
        refreshes
            .into_iter()
            .map(wire_name)
            .collect::<BTreeSet<_>>()
    );

    let causes = [
        C::AccuracyTargetMissed,
        C::CountMinWeightsNotProvenNonNegative,
        C::NoAccuracyGuarantee,
        C::LatencyBoundExceeded,
        C::LatencyOverRepeatInterval,
        C::OverClusterCapacity,
        C::Costlier,
        C::Slower,
        C::MoreMemory,
        C::TiedLostOnEnumerationOrder,
    ];
    for cause in causes {
        match cause {
            C::AccuracyTargetMissed
            | C::CountMinWeightsNotProvenNonNegative
            | C::NoAccuracyGuarantee
            | C::LatencyBoundExceeded
            | C::LatencyOverRepeatInterval
            | C::OverClusterCapacity
            | C::Costlier
            | C::Slower
            | C::MoreMemory
            | C::TiedLostOnEnumerationOrder => {}
        }
    }
    let failed: BTreeSet<String> = causes
        .into_iter()
        .filter(|c| c.failed_a_check())
        .map(wire_name)
        .collect();
    let lost: BTreeSet<String> = causes
        .into_iter()
        .filter(|c| !c.failed_a_check())
        .map(wire_name)
        .collect();
    assert_eq!(stages_js_strings("const FAILED_CHECK_CAUSES = ["), failed);
    assert_eq!(
        stages_js_strings("const REJECTION_CAUSES = FAILED_CHECK_CAUSES.concat(["),
        lost
    );
}
