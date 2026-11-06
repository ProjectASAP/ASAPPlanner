//! Issue #171 — composing exact operators with summary plans across
//! explicit update/evaluation boundaries, through the planner stages'
//! logical candidates (Stage 1).
//!
//! Covers grouped fine-to-coarse and identity folds over a summary
//! evaluation, phase-aware summary construction and pre/post-ASAP schemas,
//! along with pins for every exact-accumulator nesting under an outer
//! sketch.

use std::rc::Rc;

use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::explicit_local_candidates;
use asap_types::ir::operator::agg_intent::{default_quantile, AggIntent};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{DataType, Field, Schema};

use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};

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
    explicit_local_candidates(Rc::clone(root))
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

// ── step 1: pin every already-supported exact-accumulator nesting ───────

// ── direction 1: outer exact fold over an inner summary evaluation ────────

// ── summary construction phase ──────────────────────────────────────────

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
