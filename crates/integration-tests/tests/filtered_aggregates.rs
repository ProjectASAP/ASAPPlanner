//! Filtered aggregates (`FILTER (WHERE …)`, a per-measure filter, or a
//! filtered `SummaryAgg`) execute with SQL semantics: a row that fails the
//! filter contributes nothing, but its group is kept.
mod executor_models;
mod physical_common;
use std::collections::BTreeMap;
use std::rc::Rc;

use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_optimizer::pass1::logical_candidates::{
    compose_logical_candidate, enumerate_choices, enumerate_local_logical_candidates,
};
use asap_plan_selection::plan_stages;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, Evidence, EvidenceSource, Predictability, QueryRecurrence, Rate,
    RootDemand,
};
use executor_models::executor_models;

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

/// The executed rows, printed and sorted (`Value` has no `PartialEq`).
fn sorted(root: &Rc<OperatorNode>) -> Vec<String> {
    let mut rows: Vec<_> = physical_common::execute_raw_rows(root, rows())
        .iter()
        .map(|row| print(row))
        .collect();
    rows.sort();
    rows
}

fn print(row: &[Value]) -> String {
    row.iter()
        .map(|v| match v {
            Value::Utf8(s) => s.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::Float64(x) => format!("{x:?}"),
            Value::Null => "NULL".into(),
            other => panic!("unexpected value {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn printed(rows: [Vec<Value>; 3]) -> Vec<String> {
    rows.iter().map(|row| print(row)).collect()
}

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

/// The selected plan's root for an exact `sql`, through all three stages.
async fn selected(sql: &str) -> Rc<OperatorNode> {
    let root = lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap();
    let demand = [RootDemand {
        accuracy: Some(AccuracyTarget::Exact),
        recurrence: QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        },
        predictability: Predictability::default(),
        latency_ms: None,
    }];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: declared(Rate(1_000.0)),
        input_cardinality: declared(1_000_000),
        ..Default::default()
    };
    let run = plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &demand,
        &data,
        executor_models(),
        4096,
    )
    .unwrap();
    let QueryRoot::Operator(root) = &run.plan.logical[0].1 else {
        panic!("operator root")
    };
    root.clone()
}

fn s(v: &str) -> Value {
    Value::Utf8(v.into())
}

/// `COUNT(*) FILTER (WHERE x > 0) GROUP BY g` counts the matching rows per
/// group, and a group without one reports 0, through `plan_stages` and the
/// executor.
#[tokio::test]
async fn exact_filtered_count_keeps_groups_without_matches() {
    let root =
        selected("SELECT g, COUNT(*) FILTER (WHERE x > 0) AS c FROM events GROUP BY g").await;
    assert_eq!(
        sorted(&root),
        printed([
            vec![s("a"), Value::Int64(2)],
            vec![s("b"), Value::Int64(0)],
            vec![s("c"), Value::Int64(1)],
        ])
    );
}

/// `SUM(x) FILTER (WHERE x > 0) GROUP BY g` sums the matching rows; a group
/// without one reports NULL, as SQL's SUM over no rows does.
#[tokio::test]
async fn exact_filtered_sum_is_null_for_groups_without_matches() {
    let root = selected("SELECT g, SUM(x) FILTER (WHERE x > 0) AS s FROM events GROUP BY g").await;
    assert_eq!(
        sorted(&root),
        printed([
            vec![s("a"), Value::Float64(3.0)],
            vec![s("b"), Value::Null],
            vec![s("c"), Value::Float64(5.0)],
        ])
    );
}

/// `COUNT(y)` over a nullable `y` is lowered as a count filtered by
/// `y IS NOT NULL`, so it executes the same way.
#[tokio::test]
async fn exact_count_of_nullable_column_skips_nulls() {
    let root = selected("SELECT g, COUNT(y) AS c FROM events GROUP BY g").await;
    assert_eq!(
        sorted(&root),
        printed([
            vec![s("a"), Value::Int64(1)],
            vec![s("b"), Value::Int64(0)],
            vec![s("c"), Value::Int64(1)],
        ])
    );
}

/// The `SummaryAgg` nodes in `root`.
fn summary_builds(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    OperatorNode::reachable(root)
        .into_iter()
        .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .collect()
}

/// `root` with each `SummaryAgg`'s filter set to `filter`.
fn with_summary_filter(
    root: &Rc<OperatorNode>,
    filter: &asap_types::ir::scalar::Predicate,
) -> Rc<OperatorNode> {
    if let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter: None,
    }) = &root.operator
    {
        let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
            child: child.clone(),
            family: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            grouping: grouping.clone(),
            filter: Some(filter.clone()),
        }))
        .unwrap();
        let state = match &root.coverage {
            Some(coverage) => state.with_coverage(coverage.clone()).unwrap(),
            None => state,
        };
        return Rc::new(state);
    }
    Rc::new(
        root.map_children(|c| with_summary_filter(c, filter))
            .unwrap(),
    )
}

/// The predicate of the filtered aggregate in `sql`, and its input schema.
async fn measure_filter(sql: &str) -> (asap_types::ir::scalar::Predicate, Schema) {
    let root = lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap();
    OperatorNode::reachable(&root)
        .into_iter()
        .find_map(|n| match n.non_asap() {
            Some(NonASAPOp::Aggregate { filters, child, .. }) => {
                Some((filters[0].clone()?, child.schema.clone()))
            }
            _ => None,
        })
        .expect("filtered aggregate")
}

/// Pass 1's alternatives for `sql` whose single `SummaryAgg` has `family`,
/// with that `SummaryAgg`'s filter set to `filter` by hand.
async fn hand_filtered(
    sql: &str,
    family: impl Fn(&asap_types::ir::schema::FieldDataType) -> bool,
    filter: &(asap_types::ir::scalar::Predicate, Schema),
) -> Vec<Rc<OperatorNode>> {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_sql(sql, &catalog(), target).await.unwrap();
    let inventory =
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root))], &BTreeMap::new())
            .unwrap();
    let mut result = vec![];
    for choice in enumerate_choices(&inventory, usize::MAX) {
        let roots = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &roots[0].1 else {
            panic!("operator root")
        };
        let builds = summary_builds(root);
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
            result.push(with_summary_filter(root, &filter.0));
        }
    }
    result
}

/// A hand-filtered exact `Count` accumulator counts only `x > 0` per `g`
/// but keeps every group: `b`, with no matching row, reads 0.
#[tokio::test]
async fn hand_filtered_exact_count_compiles_and_executes() {
    use asap_types::ir::schema::{ExactKind, FieldDataType};
    let filter =
        measure_filter("SELECT g, COUNT(*) FILTER (WHERE x > 0) AS c FROM events GROUP BY g").await;
    let roots = hand_filtered(
        "SELECT g, COUNT(*) AS c FROM events GROUP BY g",
        |f| matches!(f, FieldDataType::ExactAggregate(ExactKind::Count, _)),
        &filter,
    )
    .await;
    assert_eq!(roots.len(), 1);
    physical_common::compile_physical_asap_dag(&roots[0]).unwrap();
    assert_eq!(
        sorted(&roots[0]),
        printed([
            vec![s("a"), Value::Int64(2)],
            vec![s("b"), Value::Int64(0)],
            vec![s("c"), Value::Int64(1)],
        ])
    );
}

/// A hand-filtered KLL sketch holds only the `x >= 0` values of each `g`
/// and keeps every group. (The unfiltered plan declares a non-null result,
/// so no group is left empty here; `physical_semantics` covers NULL.)
#[tokio::test]
async fn hand_filtered_kll_compiles_and_executes() {
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
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
        physical_common::compile_physical_asap_dag(&root).unwrap();
        assert_eq!(
            sorted(&root),
            printed([
                vec![s("a"), Value::Float64(1.0)],
                vec![s("b"), Value::Float64(0.0)],
                vec![s("c"), Value::Float64(5.0)],
            ])
        );
    }
}

/// Whether `root` binds in the executor against an in-memory `events`.
fn binds(root: &Rc<OperatorNode>) -> Result<(), String> {
    use asap_executor::sources::{DataSources, MemorySource};
    use asap_executor::values::Batch;
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPOperatorPayload};
    let wire = physical_common::compile_physical_asap_dag(root).map_err(|e| e.to_string())?;
    let (source, schema) = wire
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::Relational {
                operator: NonASAPOpKind::Scan { source, .. },
            } => Some((source.clone(), node.output_schema.clone())),
            _ => None,
        })
        .unwrap();
    let schema = std::sync::Arc::new(schema);
    let batch = Batch::try_new(schema.clone(), vec![]).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source,
            std::sync::Arc::new(MemorySource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();
    asap_executor::physical_planner::bind_with_data_sources(
        &wire,
        BTreeMap::new(),
        &[u64::from(wire.roots[0].0)],
        &sources,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Every Pass 1 alternative for `sql` at `target`, composed, with the
/// families of its `SummaryAgg`s and whether they all carry a filter.
async fn alternatives(sql: &str, target: AccuracyTarget) -> Vec<(String, Rc<OperatorNode>)> {
    let root = lower_sql(sql, &catalog(), target).await.unwrap();
    let inventory =
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root))], &BTreeMap::new())
            .unwrap();
    enumerate_choices(&inventory, usize::MAX)
        .into_iter()
        .map(|choice| {
            let roots = compose_logical_candidate(&inventory, &choice)
                .unwrap_or_else(|e| panic!("{choice:?} composes: {e}"));
            let QueryRoot::Operator(root) = &roots[0].1 else {
                panic!("operator root")
            };
            let label = summary_builds(root)
                .iter()
                .map(|build| {
                    let Some(ASAPOp::SummaryAgg {
                        family,
                        grouping,
                        filter,
                        ..
                    }) = build.asap()
                    else {
                        unreachable!()
                    };
                    assert!(filter.is_some(), "{choice:?} keeps the filter");
                    let family = match family {
                        asap_types::ir::schema::FieldDataType::Sketch(kind, _) => {
                            format!("{:?}", kind.algorithm())
                        }
                        other => format!("{other:?}"),
                    };
                    match grouping == &Default::default() {
                        true => family,
                        false => format!("Hydra{family}"),
                    }
                })
                .collect::<Vec<_>>()
                .join("+");
            (label, root.clone())
        })
        .collect()
}

/// Pass 1 offers a filtered single-measure aggregate the same alternatives
/// as the unfiltered one, each with `SummaryAgg.filter` set. All compose;
/// an alternative binds in the executor exactly when its unfiltered
/// counterpart does. Every alternative executes to the exact plan's
/// counts, `b` included.
#[tokio::test]
async fn pass1_offers_filtered_count_alternatives() {
    // ε = 0.1 keeps the Hydra grid inside the default memory limit.
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.1,
        delta: 0.01,
    };
    let filtered = alternatives(
        "SELECT g, COUNT(*) FILTER (WHERE x > 0) AS c FROM events GROUP BY g",
        target.clone(),
    )
    .await;
    let plain = lower_sql(
        "SELECT g, COUNT(*) AS c FROM events GROUP BY g",
        &catalog(),
        target.clone(),
    )
    .await
    .unwrap();
    let inventory =
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(plain))], &BTreeMap::new())
            .unwrap();
    let plain: Vec<_> = enumerate_choices(&inventory, usize::MAX)
        .into_iter()
        .map(|choice| {
            let roots = compose_logical_candidate(&inventory, &choice).unwrap();
            let QueryRoot::Operator(root) = &roots[0].1 else {
                panic!("operator root")
            };
            root.clone()
        })
        .collect();
    let labels: Vec<_> = filtered.iter().map(|(label, _)| label.as_str()).collect();
    assert_eq!(labels, ["", "ExactAggregate(Count, Count)", "HydraCms"]);
    let expected = printed([
        vec![s("a"), Value::Int64(2)],
        vec![s("b"), Value::Int64(0)],
        vec![s("c"), Value::Int64(1)],
    ]);
    for ((label, root), plain) in filtered.iter().zip(&plain) {
        assert_eq!(binds(root).is_ok(), binds(plain).is_ok(), "{label}");
        // Few groups in a wide grid: Hydra's estimate is exact here.
        assert_eq!(sorted(root), expected, "{label}");
    }
}

/// A filtered SUM and a filtered percentile: every alternative composes
/// with the filter, and the exact `Sum` accumulator, KLL and DDSketch
/// execute to the exact answer, reading NULL for `b`, which has no `x > 0`
/// row.
#[tokio::test]
async fn pass1_filtered_sum_and_quantile_alternatives_execute() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    // SQL's exact percentile has no native implementation.
    for (sql, expected, executable) in [
        (
            "SELECT g, SUM(x) FILTER (WHERE x > 0) AS v FROM events GROUP BY g",
            ["a 3.0", "b NULL", "c 5.0"],
            vec!["", "ExactAggregate(Sum, Sum)"],
        ),
        (
            "SELECT g, approx_percentile_cont(x, 0.5) FILTER (WHERE x > 0) AS v FROM events GROUP BY g",
            ["a 1.0", "b NULL", "c 5.0"],
            vec!["Kll", "DDSketch"],
        ),
    ] {
        let alternatives = alternatives(sql, target.clone()).await;
        let mut executed = vec![];
        for (label, root) in &alternatives {
            if binds(root).is_ok() {
                let actual = sorted(root);
                // Within 2%: DDSketch's relative-error guarantee at ε = 0.01.
                let close = |a: &str, e: &str| {
                    a == e
                        || matches!((a.parse::<f64>(), e.parse::<f64>()),
                            (Ok(a), Ok(e)) if (a - e).abs() <= 0.02 * e.abs())
                };
                assert!(
                    actual.len() == expected.len()
                        && actual.iter().zip(expected).all(|(a, e)| {
                            a.split(' ').zip(e.split(' ')).all(|(a, e)| close(a, e))
                        }),
                    "{sql}: {label}: {actual:?}"
                );
                executed.push(label.as_str());
            }
        }
        assert_eq!(executed, executable, "{sql}");
    }
}
