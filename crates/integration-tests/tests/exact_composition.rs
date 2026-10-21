//! Issue #171 — composing exact operators with summary plans across
//! explicit update/evaluation boundaries, through the planner stages'
//! logical candidates (Stage 1).
//!
//! Covers grouped fine-to-coarse and identity folds over a summary
//! evaluation, phase-aware summary construction and pre/post-ASAP schemas,
//! along with pins for every exact-accumulator nesting under an outer
//! sketch.

use std::rc::Rc;

use asap_integration_tests::physical_export::{at_ingestion, physical_asap_dag, timed};
use asap_integration_tests::planning::logical_candidates;
use asap_types::ir::operator::agg_intent::{default_quantile, AggIntent};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{ExactKind, FieldDataType, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};

use asap_types::ir::properties::AccuracyTarget;

// ── fixtures ────────────────────────────────────────────────────────────

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(op))
        .expect("fixture node derives its schema")
}

fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(columns, 0, vec![]),
    })
}

fn agg(by: Vec<usize>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::by(by),
        measures: vec![intent],
        output_names: vec![],
        filters: vec![],
        having: None,
        child,
    })
}

fn per_entity(intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![intent],
        output_names: vec![],
        filters: vec![],
        having: None,
        child,
    })
}

/// `quantile by (zone, host) (latency)` — the fine-grained inner summary.
fn fine_quantile() -> Rc<OperatorNode> {
    agg(
        vec![2, 3],
        default_quantile(0.99),
        metric_scan(&["zone", "host"]),
    )
}

/// The Stage 1 logical candidate of the single query `root` (planned at
/// ε = 0.01) whose outer aggregate stays exact over the inner summary
/// evaluation: the read-time composition.
fn composed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    logical_candidates(Rc::clone(root), AccuracyTarget::max_error(0.01))
        .into_iter()
        .find(|candidate| {
            !matches!(
                below_projects(candidate).asap(),
                Some(ASAPOp::SummaryEstimate { .. })
            ) && summary_evaluation(candidate).is_some()
        })
        .expect("an exact fold over a summary evaluation")
}

/// `node` with every `Project` above it skipped: Stage 1 restores a
/// rewritten aggregate's output names with a `Project`.
fn below_projects(mut node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    while let Some(NonASAPOp::Project { child, .. }) = node.non_asap() {
        node = child;
    }
    node
}

/// The first summary evaluation in `node`'s DAG.
fn summary_evaluation(node: &Rc<OperatorNode>) -> Option<&Rc<OperatorNode>> {
    if matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })) {
        return Some(node);
    }
    node.children().into_iter().find_map(summary_evaluation)
}

fn is_plain(node: &OperatorNode) -> bool {
    node.schema
        .fields
        .iter()
        .all(|f| matches!(f.dtype, FieldDataType::Plain(_)))
}

fn names(node: &OperatorNode) -> Vec<&str> {
    node.schema.fields.iter().map(|f| f.name.as_str()).collect()
}

// ── step 1: pin every already-supported exact-accumulator nesting ───────

#[test]
fn every_exact_accumulator_is_finalized_before_an_outer_sketch() {
    use std::time::Duration;
    let cases: Vec<(Rc<OperatorNode>, ExactKind)> = vec![
        (
            agg(
                vec![2],
                AggIntent::Sum { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Sum,
        ),
        (
            agg(
                vec![2],
                AggIntent::Count {
                    accuracy: AccuracyTarget::exact(),
                },
                metric_scan(&["zone"]),
            ),
            ExactKind::Count,
        ),
        (
            agg(
                vec![2],
                AggIntent::Min { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Min,
        ),
        (
            agg(
                vec![2],
                AggIntent::Max { col: None },
                metric_scan(&["zone"]),
            ),
            ExactKind::Max,
        ),
        (
            per_entity(
                AggIntent::Rate,
                node(NonASAPOp::TimeRange {
                    range: Duration::from_secs(300),
                    kind: TimeRangeKind::Range,
                    child: metric_scan(&["zone"]),
                }),
            ),
            ExactKind::Rate,
        ),
        (
            per_entity(
                AggIntent::Increase,
                node(NonASAPOp::TimeRange {
                    range: Duration::from_secs(300),
                    kind: TimeRangeKind::Range,
                    child: metric_scan(&["zone"]),
                }),
            ),
            ExactKind::Increase,
        ),
    ];
    for (inner, kind) in cases {
        let outer = agg(vec![], default_quantile(0.9), inner);
        // The KLL candidate over the inner exact accumulator.
        let root = logical_candidates(Rc::clone(&outer), AccuracyTarget::max_error(0.01))
            .into_iter()
            .map(|candidate| Rc::clone(below_projects(&candidate)))
            .find(|candidate| {
                matches!(candidate.asap(), Some(ASAPOp::SummaryEstimate { summary_input, .. })
                    if matches!(summary_input.asap(), Some(ASAPOp::SummaryAgg { child, .. })
                        if matches!(child.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))))
            })
            .unwrap_or_else(|| panic!("{kind:?}: a KLL over the exact accumulator"));
        // Timing is not stored on the plan: time it with the outer summary
        // maintained (which also validates every edge) and inspect the copy.
        let root = at_ingestion(&root);
        let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &root.operator else {
            panic!("expected KLL evaluation, got {:?}", root.operator);
        };
        let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary_input.operator else {
            panic!("expected outer SummaryAgg");
        };
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: finalized }) = &child.operator
        else {
            panic!("{kind:?}: missing maintenance finalization");
        };
        assert_eq!(
            child.timing,
            Some(ExecutionTiming::IngestionTime),
            "{kind:?}: finalization runs at maintenance time"
        );
        assert!(
            matches!(
                &finalized.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(k, _), .. }) if *k == kind
            ),
            "{kind:?}: expected the exact accumulator under its finalization, got {:?}",
            finalized.operator
        );
    }
}

// ── direction 1: outer exact fold over an inner summary evaluation ────────

/// `max`/`avg` over a quantile composes as an exact fold at query time over
/// the inner quantile's evaluation, with the pre-ASAP target's own schema.
#[test]
fn max_and_avg_over_quantile_compose_at_query_time() {
    for intent in [AggIntent::Max { col: None }, AggIntent::Avg { col: None }] {
        let root = agg(vec![0], intent.clone(), fine_quantile());
        let composed = composed(&root);
        let child = summary_evaluation(&composed).unwrap();
        assert!(
            child.guarantee.is_some(),
            "child has its KLL rank guarantee"
        );
        assert!(
            composed.guarantee.is_none(),
            "rank error has no definition-backed conversion through max/average"
        );
        assert!(is_plain(&composed));
        assert_eq!(
            names(&composed),
            root.schema
                .fields
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "the composed plan's schema is the pre-ASAP target's own"
        );
        assert_eq!(
            timed(&composed).timing,
            Some(ExecutionTiming::QueryTime),
            "{intent:?}: the exact fold runs at query time"
        );
    }
}

/// Grouped fine-to-coarse fold (`by (zone)` over `by (zone, host)`) and the
/// identity fold (`by (zone)` over `by (zone)`) both compose; the operator
/// is the same, only the fold's row multiplicity differs.
#[test]
fn identity_and_genuine_multi_row_folds_both_compose() {
    let identity_inner = agg(vec![2], default_quantile(0.99), metric_scan(&["zone"]));
    for (label, inner) in [
        ("identity", identity_inner),
        ("fine-to-coarse", fine_quantile()),
    ] {
        let root = agg(vec![0], AggIntent::Max { col: None }, inner);
        let composed = composed(&root);
        assert_eq!(
            timed(&composed).timing,
            Some(ExecutionTiming::QueryTime),
            "{label}"
        );
        assert_eq!(names(&composed), vec!["zone", "max"], "{label}");
    }
}

// ── summary construction phase ──────────────────────────────────────────

/// Summary construction can consume query-time values without pretending
/// they are available to an ingestion-time consumer.
#[test]
fn summary_construction_follows_its_value_input_phase() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let post = composed(&root);
    let illegal = std::rc::Rc::new(
        OperatorNode::with_schema(
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                child: post,
                family: FieldDataType::ExactAggregate(
                    ExactKind::Max,
                    asap_types::ir::schema::ExactParams::Max,
                ),
                input: SummaryUpdate::column(asap_types::ir::scalar::ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: Default::default(),
                filter: None,
            }),
            Schema::lifted(vec![], None),
        )
        .with_guarantee(None),
    );
    // Even under an ingestion-time consumer the state is built at query
    // time, because a evaluation sits below it.
    let state = asap_types::ir::planned_data_state(&illegal, ExecutionTiming::IngestionTime);
    assert_eq!(state.timing, ExecutionTiming::QueryTime);
    asap_types::ir::validate_at_ingestion(&illegal, state.timing).unwrap();
}

// ── Physical export: explicit timing ─────────────────────────────────────

#[test]
fn physical_export_carries_explicit_timing_for_a_composed_plan() {
    let root = agg(vec![0], AggIntent::Max { col: None }, fine_quantile());
    let composed = composed(&root);
    // Timing is explicit in the wire-6 DAG: the root finalizes the exact
    // `max` state over the quantile evaluation, at query time.
    let wire = physical_asap_dag(&composed);
    let wire_root = wire.nodes.iter().find(|n| n.id == wire.roots[0]).unwrap();
    assert!(matches!(
        wire_root.payload,
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::FinalizeExactAccumulator { .. })
    ));
    assert_eq!(wire_root.output_state.timing, ExecutionTiming::QueryTime);
}
