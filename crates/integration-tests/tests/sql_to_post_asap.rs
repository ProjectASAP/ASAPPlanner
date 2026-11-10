//! End-to-end SQL query-string → post-ASAP IR pin (issue #191).
//!
//! The SQL counterpart of `promql_to_post_asap.rs`: drives SQL text —
//! `lower_sql` (text → non-ASAP `OperatorNode` tree) → the planner stages'
//! logical candidates (→ a tree with ASAP operators, see [`realize`]
//! below) — and pins the resulting sketch-vs-exact-accumulator shape node by
//! node, the way `promql_to_post_asap.rs` does for PromQL.
//!
//! ## A structural wrinkle PromQL doesn't have
//!
//! `lower_promql` returns a *bare* `NonASAPOp::Aggregate` for a top-level
//! aggregation (`sum by (job) (m)`, `quantile(0.99, …)`), so a summary can
//! bind it directly at the DAG root. `lower_sql` never does: DataFusion's
//! planner always wraps even a single, unaliased aggregate in an identity
//! `Project` (confirmed below), so a SQL DAG's *root* is normally `Project {
//! child: Aggregate { .. } }`. Final materialization retains that projection
//! as a query-time non-ASAP node and independently plans its child, keeping
//! both SELECT-list semantics and the summary-bound aggregate visible.

use std::rc::Rc;

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::logical_candidates_ignoring_guarantees;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, Predicate};
use asap_types::plan::physical_asap_dag::{PhysicalASAPNodeId, PhysicalASAPOperatorPayload};

/// [`realize`] with every output accepted whatever its guarantee: a value
/// filter over an estimate has no guarantee, so Pass 1 prunes the summary
/// these pins inspect unless guarantees are ignored.
fn realize_ignoring_guarantees(
    root: &Rc<OperatorNode>,
    accuracy: AccuracyTarget,
) -> Rc<OperatorNode> {
    first_summary_bound(logical_candidates_ignoring_guarantees(
        Rc::clone(root),
        accuracy,
    ))
}

/// The first of `candidates` that uses an ASAP operator, or the first.
fn first_summary_bound(candidates: Vec<Rc<OperatorNode>>) -> Rc<OperatorNode> {
    let node = candidates
        .iter()
        .find(|candidate| candidate.contains_asap())
        .or(candidates.first())
        .cloned()
        .expect("Stage 1 offers at least the exact candidate");
    node.validate_structure()
        .expect("planned dag satisfies the unified IR contract");
    node
}

/// The single input of a unary non-ASAP node (Project, Filter, Sort, ...) or
/// of a `FinalizeExactAccumulator`; `None` for anything else.
fn unary_child(node: &OperatorNode) -> Option<&Rc<OperatorNode>> {
    match &node.operator {
        Operator::NonASAP(_) => match node.children().as_slice() {
            [child] => Some(*child),
            _ => None,
        },
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => Some(child),
        Operator::ASAP(_) => None,
    }
}

/// A sub-DAG kept as plain (non-ASAP) work: no ASAP operator anywhere below.
fn is_kept_non_asap(node: &OperatorNode) -> bool {
    node.non_asap().is_some() && !node.contains_asap()
}

/// Mirror a scalar-only predicate (no operator references) to its wire form.
fn wire_pred(pred: &Predicate) -> Predicate<PhysicalASAPNodeId> {
    Predicate(pred.0.map_operator_refs(&mut |_| -> PhysicalASAPNodeId {
        panic!("fixture predicate references no operator")
    }))
}

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

/// `metrics(ts, service, latency, bytes)` — mirrors
/// `frontend-sql/tests/sql_lowering.rs`'s catalog.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("latency", DataType::Float64),
                col("bytes", DataType::Int64),
            ],
            0,
            vec![vec![0, 1]],
        ),
    )
}

async fn lower(sql: &str) -> Rc<OperatorNode> {
    lower_sql(sql, &catalog())
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

#[tokio::test]
async fn sql_relational_parents_retain_summary_bound_aggregate() {
    let pre_asap = lower(
        "SELECT t.service, t.p FROM \
         (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
          FROM metrics GROUP BY service) t \
         WHERE t.p > 100 ORDER BY t.p DESC LIMIT 5",
    )
    .await;
    let root = realize_ignoring_guarantees(&pre_asap, AccuracyTarget::max_error(0.01));

    let mut node = root.as_ref();
    let mut saw_project = false;
    let mut saw_filter = false;
    let mut saw_sort = false;
    let mut saw_limit = false;
    loop {
        if let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator {
            assert!(matches!(
                summary_input.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { .. })
            ));
            break;
        }
        match node.non_asap() {
            Some(NonASAPOp::Project { .. }) => saw_project = true,
            Some(NonASAPOp::Filter { .. }) => saw_filter = true,
            Some(NonASAPOp::Sort { .. }) => saw_sort = true,
            Some(NonASAPOp::Limit { n, offset, .. }) => {
                assert_eq!((*n, *offset), (Some(5), 0));
                saw_limit = true;
            }
            _ => {}
        }
        node = unary_child(node).unwrap_or_else(|| {
            panic!(
                "expected relational parents over SummaryEstimate, got {:?}",
                node.operator
            )
        });
    }
    assert!(saw_project && saw_filter && saw_sort && saw_limit);
}

/// A post-aggregate predicate is read-time work, while a source predicate is
/// part of the rows that populate the maintained summary. Neither predicate
/// may be dropped or moved across the aggregation boundary.
#[tokio::test]
async fn sql_filter_keeps_read_predicate_and_summary_population_selection() {
    let pre_asap = lower(
        "SELECT t.service, t.p FROM \
         (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
          FROM metrics WHERE service = 'api' GROUP BY service) t \
         WHERE t.p > 100",
    )
    .await;
    let expected_read_predicate = {
        let mut node = &pre_asap;
        loop {
            match node.non_asap() {
                Some(NonASAPOp::Filter { pred, .. }) => break pred.clone(),
                Some(
                    NonASAPOp::Project { child, .. }
                    | NonASAPOp::Sort { child, .. }
                    | NonASAPOp::Limit { child, .. },
                ) => node = child,
                _ => panic!(
                    "expected a Filter above the aggregate, got {:?}",
                    node.operator
                ),
            }
        }
    };
    let expected_source_predicates = {
        let mut node = &pre_asap;
        loop {
            match node.non_asap() {
                Some(NonASAPOp::Scan { predicates, .. }) => break predicates.clone(),
                Some(
                    NonASAPOp::Project { child, .. }
                    | NonASAPOp::Filter { child, .. }
                    | NonASAPOp::Aggregate { child, .. }
                    | NonASAPOp::Sort { child, .. }
                    | NonASAPOp::Limit { child, .. },
                ) => node = child,
                _ => panic!(
                    "expected a unary SQL plan over Scan, got {:?}",
                    node.operator
                ),
            }
        }
    };
    assert_eq!(expected_source_predicates.len(), 1, "fixture source WHERE");

    let root = realize_ignoring_guarantees(&pre_asap, AccuracyTarget::max_error(0.01));

    let mut node = root.as_ref();
    let mut retained_read_predicate = None;
    loop {
        if let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator {
            let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary_input.operator else {
                panic!("expected SummaryAgg below SummaryEstimate");
            };
            assert!(
                is_kept_non_asap(child),
                "expected raw summary population below SummaryAgg"
            );
            let Some(NonASAPOp::Scan { predicates, .. }) = child.non_asap() else {
                panic!("expected source selection to remain a Scan");
            };
            assert_eq!(predicates, &expected_source_predicates);
            break;
        }
        if let Some(NonASAPOp::Filter { pred, .. }) = node.non_asap() {
            retained_read_predicate = Some(pred.clone());
        }
        node = unary_child(node).unwrap_or_else(|| {
            panic!(
                "expected read-time operations over a summary, got {:?}",
                node.operator
            )
        });
    }
    assert_eq!(retained_read_predicate, Some(expected_read_predicate));

    let dag = physical_asap_dag(&root);
    let expected_wire = wire_pred(retained_read_predicate.as_ref().unwrap());
    assert!(dag.nodes.iter().any(|node| matches!(
        &node.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Filter { pred, .. }) if *pred == expected_wire
    )));
}
