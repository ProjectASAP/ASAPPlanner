//! Filtered aggregates (`FILTER (WHERE …)`, a per-measure filter, or a
//! filtered `SummaryAgg`) execute with SQL semantics: a row that fails the
//! filter contributes nothing, but its group is kept.
mod physical_common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::planning::{logical_candidates, selected_dag};
use asap_physical_operators::{
    physical_planner::{compile, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    sources::{DataSources, MemorySource},
    values::{Batch, Value},
};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, Predicate};
use asap_types::post_asap::{ExactKind, FieldDataType, SketchAlgorithm};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::RequestedAccuracy;
use futures::StreamExt;

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "events",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("g", DataType::Utf8, false),
            Field::plain("x", DataType::Float64, false),
            Field::plain("y", DataType::Float64, true),
        ]),
    )
}

/// Group `b` has rows but none with `x > 0`, and no non-NULL `y`.
fn rows() -> Vec<Vec<Value>> {
    [
        ("a", 1.0, Some(1.0)),
        ("a", -1.0, None),
        ("a", 2.0, None),
        ("b", -3.0, None),
        ("b", 0.0, None),
        ("c", 5.0, Some(7.0)),
    ]
    .into_iter()
    .map(|(g, x, y)| {
        vec![
            Value::Timestamp(0),
            Value::Utf8(g.into()),
            Value::Float64(x),
            y.map_or(Value::Null, Value::Float64),
        ]
    })
    .collect()
}

/// Run the logical DAG `root` over [`rows`]; rows come back shown and sorted.
fn execute(root: &Rc<OperatorNode>) -> Vec<String> {
    let dag = physical_common::compile_physical_asap_dag(root).unwrap();
    let scan = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            )
        })
        .expect("one scan");
    let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) = &scan.payload else {
        unreachable!()
    };
    let schema = Arc::new(scan.output_schema.clone());
    let plan = compile(
        &dag,
        BTreeMap::from([(scan.id as u64, InputContract::bounded(schema.clone()))]),
        &[dag.roots[0] as u64],
    )
    .unwrap();
    let expression = OperatorNode::reachable(root)
        .into_iter()
        .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { source: s, .. }) if s == source))
        .unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source.clone(),
            Arc::new(
                MemorySource::new(
                    schema.clone(),
                    vec![Batch::try_new(schema.clone(), rows()).unwrap()],
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let bound = plan
        .instantiate(BTreeMap::from([(
            scan.id as u64,
            Box::new(sources.bind(&expression).unwrap()) as Source<'_>,
        )]))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let show = |value: &Value| match value {
        Value::Null => "NULL".to_string(),
        Value::Int64(value) => value.to_string(),
        Value::Float64(value) => format!("{value:?}"),
        Value::Utf8(value) => value.to_string(),
        other => format!("{other:?}"),
    };
    let mut out: Vec<String> = futures::executor::block_on(async {
        let mut stream = bound.execute(plan.roots(), context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(
                batch
                    .unwrap()
                    .rows()
                    .iter()
                    .map(|row| row.iter().map(show).collect::<Vec<_>>().join(" ")),
            );
        }
        rows
    });
    out.sort();
    out
}

async fn lower(sql: &str, accuracy: RequestedAccuracy) -> Rc<OperatorNode> {
    lower_sql(sql, &catalog(), accuracy).await.unwrap()
}

/// The selected plan for an exact `sql`, through all three stages.
async fn exact_plan(sql: &str) -> Rc<OperatorNode> {
    selected_dag(
        lower(sql, RequestedAccuracy::exact()).await,
        RequestedAccuracy::exact(),
    )
}

/// `COUNT(*) FILTER (WHERE x > 0) GROUP BY g` counts the matching rows per
/// group, and a group without one reports 0.
#[tokio::test]
async fn exact_filtered_count_keeps_groups_without_matches() {
    let root =
        exact_plan("SELECT g, COUNT(*) FILTER (WHERE x > 0) AS c FROM events GROUP BY g").await;
    assert_eq!(execute(&root), ["a 2", "b 0", "c 1"]);
}

/// `SUM(x) FILTER (WHERE x > 0) GROUP BY g` sums the matching rows; a group
/// without one reports NULL, as SQL's SUM over no rows does.
#[tokio::test]
async fn exact_filtered_sum_is_null_for_groups_without_matches() {
    let root =
        exact_plan("SELECT g, SUM(x) FILTER (WHERE x > 0) AS s FROM events GROUP BY g").await;
    assert_eq!(execute(&root), ["a 3.0", "b NULL", "c 5.0"]);
}

/// `COUNT(y)` over a nullable `y` is lowered as a count filtered by
/// `y IS NOT NULL`, so it executes the same way.
#[tokio::test]
async fn exact_count_of_nullable_column_skips_nulls() {
    let root = exact_plan("SELECT g, COUNT(y) AS c FROM events GROUP BY g").await;
    assert_eq!(execute(&root), ["a 1", "b 0", "c 1"]);
}

/// `root` with each unfiltered `SummaryAgg`'s filter set to `filter`.
fn with_summary_filter(root: &Rc<OperatorNode>, filter: &Predicate) -> Rc<OperatorNode> {
    if let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter: None,
    }) = &root.operator
    {
        return Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
                child: child.clone(),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: Some(filter.clone()),
            }))
            .unwrap(),
        );
    }
    Rc::new(
        root.with_new_children(|c| with_summary_filter(c, filter))
            .unwrap(),
    )
}

/// The predicate of the filtered aggregate in `sql`, and its input schema.
async fn measure_filter(sql: &str) -> (Predicate, Schema) {
    OperatorNode::reachable(&lower(sql, RequestedAccuracy::exact()).await)
        .into_iter()
        .find_map(|n| match n.non_asap() {
            Some(NonASAPOp::Aggregate { filters, child, .. }) => {
                Some((filters[0].clone()?, child.schema.clone()))
            }
            _ => None,
        })
        .expect("filtered aggregate")
}

/// The logical candidates for `sql` whose single `SummaryAgg` has a family
/// matching `family`, with that `SummaryAgg`'s filter set by hand.
async fn hand_filtered(
    sql: &str,
    family: impl Fn(&FieldDataType) -> bool,
    filter: &(Predicate, Schema),
) -> Vec<Rc<OperatorNode>> {
    let target = RequestedAccuracy::epsilon_delta(0.01, 0.01);
    let mut result = vec![];
    for root in logical_candidates(lower(sql, target.clone()).await, target) {
        let builds: Vec<_> = OperatorNode::reachable(&root)
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .collect();
        let [build] = builds.as_slice() else {
            continue;
        };
        let Some(ASAPOp::SummaryAgg {
            family: actual,
            child,
            ..
        }) = build.asap()
        else {
            unreachable!()
        };
        if family(actual) {
            // The filter's column indices refer to the same input relation.
            assert_eq!(child.schema, filter.1);
            result.push(with_summary_filter(&root, &filter.0));
        }
    }
    result
}

/// A hand-filtered exact `Count` accumulator counts only `x > 0` per `g`
/// but keeps every group: `b`, with no matching row, reads 0.
#[tokio::test]
async fn hand_filtered_exact_count_compiles_and_executes() {
    let filter =
        measure_filter("SELECT g, COUNT(*) FILTER (WHERE x > 0) AS c FROM events GROUP BY g").await;
    let roots = hand_filtered(
        "SELECT g, COUNT(*) AS c FROM events GROUP BY g",
        |f| matches!(f, FieldDataType::ExactAggregate(ExactKind::Count, _)),
        &filter,
    )
    .await;
    assert!(!roots.is_empty());
    for root in roots {
        assert_eq!(execute(&root), ["a 2", "b 0", "c 1"]);
    }
}

/// A hand-filtered KLL sketch holds only the `x >= 0` values of each `g`
/// and keeps every group. (The unfiltered plan declares a non-null result,
/// so no group is left empty here; `physical_semantics` covers NULL.)
#[tokio::test]
async fn hand_filtered_kll_compiles_and_executes() {
    let filter = measure_filter(
        "SELECT g, approx_percentile_cont(x, 0.5) FILTER (WHERE x >= 0) AS q FROM events GROUP BY g",
    )
    .await;
    let roots = hand_filtered(
        "SELECT g, approx_percentile_cont(x, 0.5) AS q FROM events GROUP BY g",
        |f| matches!(f, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::Kll),
        &filter,
    )
    .await;
    assert!(!roots.is_empty());
    for root in roots {
        assert_eq!(execute(&root), ["a 1.0", "b 0.0", "c 5.0"]);
    }
}

/// The KLL candidates of `sql`, unchanged.
async fn kll_candidates(sql: &str) -> Vec<Rc<OperatorNode>> {
    let target = RequestedAccuracy::epsilon_delta(0.01, 0.01);
    let roots: Vec<_> = logical_candidates(lower(sql, target.clone()).await, target)
        .into_iter()
        .filter(|root| {
            OperatorNode::reachable(root).iter().any(|n| {
                matches!(n.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                    if kind.algorithm() == &SketchAlgorithm::Kll)
            })
        })
        .collect();
    assert!(!roots.is_empty(), "no KLL candidate for {sql}");
    roots
}

/// A KLL quantile of a nullable column is NULL for a group whose values are
/// all NULL (`b`), as SQL's quantile over no value is, instead of failing
/// the query.
#[tokio::test]
async fn kll_quantile_of_all_null_group_is_null() {
    for root in
        kll_candidates("SELECT g, approx_percentile_cont(y, 0.5) AS q FROM events GROUP BY g").await
    {
        assert_eq!(execute(&root), ["a 1.0", "b NULL", "c 7.0"]);
    }
}

/// An ungrouped KLL quantile over no row is one NULL row.
#[tokio::test]
async fn global_kll_quantile_over_no_row_is_null() {
    for root in
        kll_candidates("SELECT approx_percentile_cont(x, 0.5) AS q FROM events WHERE x > 100").await
    {
        assert_eq!(execute(&root), ["NULL"]);
    }
}
