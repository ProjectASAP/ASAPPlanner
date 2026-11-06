//! Fixtures this crate's own tests share. Every builder returns an
//! `Rc<OperatorNode>` whose schema is derived by `OperatorNode::new_shared`,
//! so a fixture is exactly what a front end would hand the planner.

use std::rc::Rc;

use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, OperatorNode};

use crate::test_support::lower_promql;

/// A `TimeSeries("m")` scan over `[ts(0), value(1), labels...]`, time index 0,
/// no unique key.
pub fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    metric_scan_with_keys(labels, vec![])
}

/// [`metric_scan`] with explicit `unique_keys` (a `[[0]]` key makes CSE
/// willing to hoist the scan).
pub fn metric_scan_with_keys(labels: &[&str], unique_keys: Vec<Vec<ColumnId>>) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    scan("m", Schema::with_time_index(columns, 0, unique_keys))
}

/// A predicate-free `TimeSeries(metric)` scan with the given schema.
pub fn scan(metric: &str, schema: Schema) -> Rc<OperatorNode> {
    scan_from(
        Source::TimeSeries {
            metric: metric.into(),
        },
        schema,
    )
}

pub fn scan_from(source: Source, schema: Schema) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema,
    }))
    .unwrap()
}

/// The Count-Min summary of a Stage 1 candidate of `query`.
pub fn cms_summary(query: &str) -> OperatorNode {
    use crate::composition::compose_logical_candidate;
    use crate::per_computation_candidates::generate_per_computation_candidates;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, QueryRoot};
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let root = lower_promql(query, target);
    // Inspect each local computation choice under the built-in deployment.
    let per_computation = generate_per_computation_candidates(
        &asap_stage0_memo::memo_of_queries(
            &[QueryRoot::Operator(root)],
            &[None],
            &asap_types::workload::DataWorkload::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .without_unsupported_summaries(None);
    let summary = per_computation
        .targets
        .iter()
        .enumerate()
        .flat_map(|(target, choices)| {
            let inventory = &per_computation;
            (0..choices.choice_count()).filter_map(move |candidate| {
                let mut assignment = vec![0; inventory.targets.len()];
                assignment[target] = candidate;
                compose_logical_candidate(inventory, &assignment).ok()
            })
        })
        .flat_map(|roots| match &roots[0] {
            QueryRoot::Operator(node) => OperatorNode::reachable(node),
            QueryRoot::Scalar(_) => vec![],
        })
        .find(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg {
                    summary_type: FieldDataType::Sketch(kind),
                    ..
                }) if *kind == SketchAlgorithm::Cms
            )
        })
        .map(|node| (*node).clone())
        .unwrap_or_else(|| panic!("{query}: a Count-Min summary"));
    summary
}

/// `summary` with its update weight replaced by `weight`.
pub fn weighted(
    mut summary: OperatorNode,
    weight: asap_types::ir::schema::SummaryInputExpr,
) -> Rc<OperatorNode> {
    use asap_types::ir::{ASAPOp, Operator};
    if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &mut summary.operator {
        input.weight = weight;
    }
    Rc::new(summary)
}

/// A point-count estimate over `summary` carrying `guarantee`.
pub fn estimate_over(
    summary: Rc<OperatorNode>,
    guarantee: asap_types::ir::properties::AccuracyGuarantee,
) -> Rc<OperatorNode> {
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::SketchStatistic;
    use asap_types::ir::{ASAPOp, Operator};
    Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: summary,
            query: SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
        }))
        .unwrap()
        .with_guarantee(Some(guarantee)),
    )
}
