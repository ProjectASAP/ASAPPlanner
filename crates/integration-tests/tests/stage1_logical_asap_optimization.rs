//! Frontends → Pass 1 → composed candidate logical ASAP DAGs: every candidate of
//! a query is a valid DAG with the query's output columns (names, types and
//! nullability).

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::{
    physical_planner::{compile, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    sources::{DataSources, MemorySource},
    values::{Batch, Value},
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{
    ExactKind, FieldDataType, SketchAlgorithm, SummaryInputExpr, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};
use futures::StreamExt;

mod physical_common;

fn accuracy() -> AccuracyTarget {
    AccuracyTarget::max_error(0.01).failure_probability(0.01)
}

async fn sql(query: &str) -> QueryRoot {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("dst", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let root = lower_sql(query, &catalog, accuracy())
        .await
        .unwrap_or_else(|e| panic!("lower failed for {query:?}: {e}"));
    QueryRoot::Operator(root)
}

fn promql(query: &str) -> QueryRoot {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: accuracy(),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1000),
        )),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload)
        .unwrap_or_else(|e| panic!("lower failed for {query:?}: {e}"))
        .remove(0)
}

fn output_columns(root: &QueryRoot) -> Vec<(String, String, bool)> {
    match root {
        QueryRoot::Operator(node) => node
            .schema
            .fields
            .iter()
            .map(|field| {
                (
                    field.name.clone(),
                    format!("{:?}", field.dtype),
                    field.nullable,
                )
            })
            .collect(),
        QueryRoot::Scalar(_) => vec![],
    }
}

/// Compose every candidate of `root`; returns how many there were.
fn compose_all(label: &str, root: QueryRoot) -> usize {
    let expected = output_columns(&root);
    let inventory = generate_per_computation_candidates(vec![(0, root)]).unwrap();
    let radix: Vec<_> = inventory
        .targets
        .iter()
        .map(|target| target.choice_count())
        .collect();
    let total: usize = radix.iter().product();
    let mut choice = vec![0; radix.len()];
    for _ in 0..total {
        let composed = compose_logical_candidate(&inventory, &choice)
            .unwrap_or_else(|e| panic!("{label} {choice:?}: {e}"));
        let root = &composed[0].1;
        root.validate_structure()
            .unwrap_or_else(|e| panic!("{label} {choice:?}: {e}"));
        assert_eq!(output_columns(root), expected, "{label} {choice:?}");
        for (digit, radix) in choice.iter_mut().zip(&radix).rev() {
            *digit += 1;
            if *digit < *radix {
                break;
            }
            *digit = 0;
        }
    }
    total
}

#[tokio::test]
async fn sql_candidates_compose() {
    for query in [
        "SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a'",
        "SELECT src, count(*) FROM flows GROUP BY src HAVING count(*) > 10",
        "SELECT src, avg(bytes), count(*), approx_distinct(dst) FROM flows GROUP BY src",
        "SELECT count(*) FILTER (WHERE bytes > 10), approx_percentile_cont(bytes, 0.99) FROM flows",
        "SELECT avg(bytes) FROM flows",
        "SELECT sum(flows.bytes) FROM flows",
        "SELECT src, approx_percentile_cont(bytes, 0.99) FILTER (WHERE bytes > 10) FROM flows GROUP BY src",
        "SELECT approx_percentile_cont(bytes, 0.99) FILTER (WHERE bytes > 10) FROM flows",
    ] {
        assert!(compose_all(query, sql(query).await) >= 1, "{query}");
    }
}

#[test]
fn promql_candidates_compose() {
    for query in [
        "avg by (job) (m)",
        "avg_over_time(m[5m])",
        "quantile(0.99, m) - quantile(0.5, m)",
        "scalar(quantile(0.99, m)) - scalar(quantile(0.5, m))",
        "topk by (job) (10, sum_over_time(m[1m]))",
        "topk(3, sum by (job) (rate(m[5m])))",
        "count by (job) (m)",
        "count_over_time(m[5m])",
        "quantile_over_time(0.99, m[5m])",
    ] {
        assert!(compose_all(query, promql(query)) >= 1, "{query}");
    }
}

/// The summary estimates of `root`'s candidates that vary one target's
/// choice, every other target taking its first.
fn composed_estimates(root: QueryRoot) -> Vec<Rc<OperatorNode>> {
    let inventory = generate_per_computation_candidates(vec![(0, root)]).unwrap();
    let targets = inventory.targets.len();
    let choices: Vec<_> = inventory
        .targets
        .iter()
        .enumerate()
        .flat_map(|(target, computation)| {
            (1..computation.choice_count()).map(move |index| {
                let mut choice = vec![0; targets];
                choice[target] = index;
                choice
            })
        })
        .collect();
    choices
        .into_iter()
        .flat_map(|choice| {
            let composed = compose_logical_candidate(&inventory, &choice).unwrap();
            let QueryRoot::Operator(node) = &composed[0].1 else {
                panic!("operator root expected")
            };
            OperatorNode::reachable(node)
        })
        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })))
        .collect()
}

fn update_of(estimate: &OperatorNode) -> (SketchAlgorithm, SummaryUpdate) {
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
        unreachable!()
    };
    let Some(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        input,
        ..
    }) = summary_input.asap()
    else {
        panic!("sketch state expected")
    };
    (kind.algorithm().clone(), input.clone())
}

/// A heap sketch's bound is per item frequency, not Top-K membership, and
/// its weights are not proven non-negative: a Top-K estimate gets no
/// guarantee. A unit-count CMS estimate keeps its bound.
#[test]
fn topk_estimates_carry_no_frequency_bound() {
    let topk = composed_estimates(promql("topk by (job) (10, sum_over_time(m[1m]))"));
    let algorithms: Vec<_> = topk.iter().map(|node| update_of(node).0).collect();
    assert_eq!(
        algorithms,
        [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
    assert!(topk.iter().all(|node| node.guarantee.is_none()));

    let count = composed_estimates(promql("count by (job) (m)"));
    let cms = count
        .iter()
        .find(|node| update_of(node).0 == SketchAlgorithm::Cms)
        .expect("CMS candidate");
    assert!(cms.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
}

/// Top-K over a grouped sum is a whole-expression heap: it absorbs the
/// sum and ranks its group keys, read from the rows the sum reads.
#[test]
fn topk_over_an_aggregate_ranks_its_labels() {
    let estimates = composed_estimates(promql("topk(3, sum by (job) (rate(m[5m])))"));
    assert_eq!(estimates.len(), 2);
    for estimate in &estimates {
        assert_eq!(
            update_of(estimate).1.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("job".into())))
        );
    }
}

/// A subquery inside a measure filter is substituted too: each choice for
/// the inner count gives its own DAG, and an approximate inner count clears
/// the outer measure's guarantee.
#[tokio::test]
async fn subquery_in_a_measure_filter_is_substituted() {
    let query = "SELECT src, count(*) FILTER (WHERE bytes > (SELECT count(*) FROM flows)) \
                 FROM flows GROUP BY src";
    let inventory = generate_per_computation_candidates(vec![(0, sql(query).await)]).unwrap();
    assert_eq!(inventory.targets.len(), 2);
    let inner = inventory
        .targets
        .iter()
        .position(|target| {
            target
                .aggregate
                .non_asap()
                .and_then(|op| match op {
                    NonASAPOp::Aggregate { reduction, .. } => reduction.group_keys(),
                    _ => None,
                })
                .is_some_and(|by| by.keys().is_empty())
        })
        .expect("inner count");
    let candidates = &inventory.targets[inner].choices.candidates;
    assert!(candidates.len() > 1);
    let (mut seen, mut approximate_inner) = (Vec::new(), false);
    for (index, candidate) in candidates.iter().enumerate() {
        let mut choice = vec![0; 2];
        choice[inner] = index;
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            panic!("operator root expected")
        };
        let shown = format!("{root:?}");
        assert!(!seen.contains(&shown), "{candidate:?} composes as another");
        seen.push(shown);
        let finalize = OperatorNode::reachable(root)
            .into_iter()
            .find(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::FinalizeExactAccumulator { child })
                        if matches!(child.asap(), Some(ASAPOp::SummaryAgg { filter: Some(_), .. }))
                )
            })
            .expect("filtered outer count");
        let Some(ASAPOp::FinalizeExactAccumulator { child }) = finalize.asap() else {
            unreachable!()
        };
        let exact = |node: &OperatorNode| node.guarantee.as_ref().is_some_and(|g| g.is_exact());
        // The state's children: its input rows, then the subquery.
        let inputs_exact = child.children().into_iter().all(|node| exact(node));
        assert_eq!(exact(&finalize), inputs_exact, "{candidate:?}");
        approximate_inner |= !inputs_exact;
    }
    assert!(approximate_inner);
}

/// Run the logical DAG `root` over `rows` of its one scanned table, each row
/// given by column name. Rows come back shown and sorted.
fn execute(root: &Rc<OperatorNode>, rows: &[Vec<(&str, Value)>]) -> Vec<Vec<String>> {
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
    let rows = rows
        .iter()
        .map(|row| {
            schema
                .fields
                .iter()
                .map(|field| {
                    row.iter()
                        .find(|(name, _)| *name == field.name)
                        .map(|(_, value)| value.clone())
                        .expect("value for every column")
                })
                .collect()
        })
        .collect();
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
                    vec![Batch::try_new(schema.clone(), rows).unwrap()],
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
        Value::Bool(value) => value.to_string(),
        Value::Float64(value) => value.to_string(),
        Value::Utf8(value) => format!("{value:?}"),
        other => format!("{other:?}"),
    };
    let mut out: Vec<Vec<String>> = futures::executor::block_on(async {
        let mut stream = bound.execute(plan.roots(), context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(
                batch
                    .unwrap()
                    .rows()
                    .iter()
                    .map(|row| row.iter().map(show).collect()),
            );
        }
        rows
    });
    out.sort();
    out
}

/// SQL `avg` from its `sum` and `count` states, composed for the query.
async fn avg_as_sum_and_count(
    catalog: &SqlCatalog,
    query: &str,
) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
    let exact = lower_sql(query, catalog, accuracy()).await.unwrap();
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(exact.clone()))]).unwrap();
    let choice: Vec<_> = inventory
        .targets
        .iter()
        .map(|target| {
            target
                .choices
                .candidates
                .iter()
                .position(|c| {
                    *c == ComputationCandidate::ExactAccumulator(vec![
                        ExactKind::Sum,
                        ExactKind::Count,
                    ])
                })
                .expect("avg as sum and count")
        })
        .collect();
    let composed =
        compose_logical_candidate(&inventory, &choice).unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryRoot::Operator(root) = &composed[0].1 else {
        panic!("operator root expected")
    };
    assert!(
        OperatorNode::reachable(root)
            .iter()
            .any(|node| matches!(node.non_asap(), Some(NonASAPOp::Join { .. }))),
        "{query}: sum joined with count expected"
    );
    assert_eq!(
        output_columns(&QueryRoot::Operator(root.clone())),
        output_columns(&QueryRoot::Operator(exact.clone())),
        "{query}"
    );
    (exact, root.clone())
}

fn avg_table() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "t",
        Schema::new(vec![
            Field::plain("k", DataType::Utf8, true),
            Field::plain("ok", DataType::Bool, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    )
}

/// SQL `avg` as `sum` and `count` states executes as the exact aggregate:
/// NULL and '' groups stay apart, a non-Utf8 key works, and an empty global
/// input averages to NULL.
#[tokio::test]
async fn sql_avg_from_sum_and_count_executes_as_avg() {
    let catalog = avg_table();
    let row = |k: Option<&str>, ok: bool, value: f64| {
        vec![
            ("k", k.map_or(Value::Null, |k| Value::Utf8(k.into()))),
            ("ok", Value::Bool(ok)),
            ("value", Value::Float64(value)),
        ]
    };
    let rows = vec![
        row(None, true, 2.),
        row(None, true, 4.),
        row(Some(""), false, 10.),
        row(Some("a"), false, 200.),
        row(Some("a"), true, 1.),
    ];
    let rows_of = |rows: &[&[&str]]| -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect()
    };
    for (query, input, want) in [
        (
            "SELECT k, avg(value) FROM t GROUP BY k",
            rows.clone(),
            rows_of(&[&["\"\"", "10"], &["\"a\"", "100.5"], &["NULL", "3"]]),
        ),
        (
            "SELECT ok, avg(value) FROM t GROUP BY ok",
            rows.clone(),
            rows_of(&[&["false", "105"], &["true", "2.3333333333333335"]]),
        ),
        (
            "SELECT avg(value) FROM t",
            rows.clone(),
            rows_of(&[&["43.4"]]),
        ),
        ("SELECT avg(value) FROM t", vec![], rows_of(&[&["NULL"]])),
    ] {
        let (exact, root) = avg_as_sum_and_count(&catalog, query).await;
        assert_eq!(execute(&exact, &input), want, "{query}");
        assert_eq!(execute(&root, &input), want, "{query}");
    }
}

/// A filtered SQL `avg` is NULL where no row passes the filter: its quotient
/// is NULL when the count is 0. The runtime executes no filtered state yet,
/// so the shape is checked.
#[tokio::test]
async fn filtered_sql_avg_is_null_without_matching_rows() {
    let catalog = avg_table();
    for query in [
        "SELECT k, avg(value) FILTER (WHERE value > 100) FROM t GROUP BY k",
        "SELECT avg(value) FILTER (WHERE value > 100) FROM t",
    ] {
        let (_, root) = avg_as_sum_and_count(&catalog, query).await;
        assert!(root.schema.fields.last().unwrap().nullable, "{query}");
        let null_when_no_count = OperatorNode::reachable(&root).iter().any(|node| {
            let Some(NonASAPOp::Project { cols, child, .. }) = node.non_asap() else {
                return false;
            };
            let count = child.schema.fields.len() - 1;
            matches!(
                &cols.last().unwrap().expr,
                ScalarExpr::Case { branches, .. } if matches!(
                    &branches[..],
                    [(ScalarExpr::Compare { left, op: CompareOpKind::Eq, right, .. },
                      ScalarExpr::Literal(ScalarValue::Null))]
                        if **left == ScalarExpr::Column(count)
                            && **right == ScalarExpr::Literal(ScalarValue::Int64(0))
                )
            )
        });
        assert!(null_when_no_count, "{query}: {root:#?}");
    }
}

/// PromQL `avg` divides its `sum` and `count` vectors with a `BinaryOp`.
#[test]
fn promql_avg_divides_sum_and_count_vectors() {
    let inventory =
        generate_per_computation_candidates(vec![(0, promql("avg by (job) (m)"))]).unwrap();
    assert_eq!(
        inventory.targets[0].choices.candidates[0],
        ComputationCandidate::ExactAccumulator(vec![ExactKind::Sum, ExactKind::Count])
    );
    let composed = compose_logical_candidate(&inventory, &[0]).unwrap();
    let QueryRoot::Operator(root) = &composed[0].1 else {
        panic!("operator root expected")
    };
    let kinds: Vec<_> = OperatorNode::reachable(root)
        .iter()
        .map(|node| node.operator.kind_name())
        .collect();
    assert!(
        kinds.contains(&"BinaryOp") && !kinds.contains(&"Join"),
        "{kinds:?}"
    );
}

/// `t(k Utf8 NULL, <columns>)` as a catalog.
fn table(columns: &[(&str, bool)]) -> SqlCatalog {
    let mut fields = vec![Field::plain("k", DataType::Utf8, true)];
    fields.extend(
        columns
            .iter()
            .map(|(name, nullable)| Field::plain(*name, DataType::Float64, *nullable)),
    );
    SqlCatalog::new().with_table("t", Schema::new(fields))
}

/// Every candidate of `query` over `catalog`, composed, by candidate.
async fn every_candidate(
    catalog: &SqlCatalog,
    query: &str,
) -> Vec<(
    ComputationCandidate,
    Result<
        Vec<(i32, QueryRoot)>,
        asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError,
    >,
)> {
    let root = lower_sql(query, catalog, accuracy()).await.unwrap();
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
    assert_eq!(inventory.targets.len(), 1, "{query}");
    inventory.targets[0]
        .choices
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            (
                candidate.clone(),
                compose_logical_candidate(&inventory, &[index]),
            )
        })
        .collect()
}

/// The update of every `count` state (exact or sketch) below `root`.
fn count_updates(root: &QueryRoot) -> Vec<SummaryUpdate> {
    let QueryRoot::Operator(root) = root else {
        panic!("operator root expected")
    };
    OperatorNode::reachable(root)
        .iter()
        .filter_map(|node| match node.asap() {
            Some(ASAPOp::SummaryAgg { family, input, .. })
                if !matches!(
                    family,
                    FieldDataType::ExactAggregate(asap_types::ir::schema::ExactKind::Sum, _)
                ) =>
            {
                Some(input.clone())
            }
            _ => None,
        })
        .collect()
}

/// Whether `update` reads only the column `name` of `t`.
fn reads_only(update: &SummaryUpdate, name: &str) -> bool {
    let column = SummaryInputExpr::Column(ColumnRef::Qualified {
        table: "t".into(),
        name: name.into(),
    });
    match &update.item {
        Some(item) => *item == column && update.weight == SummaryInputExpr::Constant(1.0),
        None => update.weight == column,
    }
}

/// `avg(c)`'s count counts `c`, not a nullable `value` column: the result is
/// the exact `avg`.
#[tokio::test]
async fn sql_avg_counts_its_own_input() {
    let catalog = table(&[("value", true), ("c", false)]);
    let query = "SELECT k, avg(c) FROM t GROUP BY k";
    let (exact, root) = avg_as_sum_and_count(&catalog, query).await;
    let row = |k: &str, value: Option<f64>, c: f64| {
        vec![
            ("k", Value::Utf8(k.into())),
            ("value", value.map_or(Value::Null, Value::Float64)),
            ("c", Value::Float64(c)),
        ]
    };
    let rows = vec![
        row("a", None, 1.),
        row("a", Some(5.), 3.),
        row("b", None, 4.),
    ];
    let want = vec![
        vec!["\"a\"".to_string(), "2".to_string()],
        vec!["\"b\"".to_string(), "4".to_string()],
    ];
    assert_eq!(execute(&exact, &rows), want);
    assert_eq!(execute(&root, &rows), want);
}

/// SQL `avg` as `sum` and `count` keeps an explicit output alias.
#[tokio::test]
async fn sql_avg_keeps_its_explicit_output_name() {
    let (_, root) =
        avg_as_sum_and_count(&avg_table(), "SELECT avg(value) AS avg_latency FROM t").await;
    assert_eq!(root.schema.fields.last().unwrap().name, "avg_latency");
}

/// SQL `avg` of a non-null Int64 column as `sum` and `count` keeps the exact plan's Float64 columns.
#[tokio::test]
async fn sql_avg_of_an_int64_column_keeps_the_exact_columns() {
    let catalog = SqlCatalog::new().with_table(
        "t",
        Schema::new(vec![
            Field::plain("k", DataType::Utf8, true),
            Field::plain("n", DataType::Int64, false),
        ]),
    );
    let query = "SELECT k, avg(n) FROM t GROUP BY k";
    let (exact, root) = avg_as_sum_and_count(&catalog, query).await;
    assert_eq!(
        root.schema.fields.last().unwrap().plain_dtype(),
        Some(&DataType::Float64)
    );
    let row = |k: &str, n: i64| vec![("k", Value::Utf8(k.into())), ("n", Value::Int64(n))];
    let rows = vec![row("a", 1), row("a", 2), row("b", 4)];
    let want = vec![
        vec!["\"a\"".to_string(), "1.5".to_string()],
        vec!["\"b\"".to_string(), "4".to_string()],
    ];
    assert_eq!(execute(&exact, &rows), want);
}

/// `count(*)` counts a non-null column, never a nullable one; executed, it
/// counts the rows whose nullable column is NULL too.
#[tokio::test]
async fn sql_count_star_counts_a_non_null_column() {
    let catalog = table(&[("v", true), ("n", false)]);
    let query = "SELECT k, count(*) FROM t GROUP BY k";
    for (candidate, composed) in every_candidate(&catalog, query).await {
        let composed = composed.unwrap_or_else(|e| panic!("{candidate:?}: {e}"));
        composed[0].1.validate_structure().unwrap();
        let updates = count_updates(&composed[0].1);
        assert_eq!(updates.len(), 1, "{candidate:?}");
        assert!(reads_only(&updates[0], "n"), "{candidate:?}: {updates:?}");
    }
    let (exact, root) = {
        let exact = lower_sql(query, &catalog, accuracy()).await.unwrap();
        let inventory =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(exact.clone()))])
                .unwrap();
        let QueryRoot::Operator(root) = compose_logical_candidate(&inventory, &[0])
            .unwrap()
            .remove(0)
            .1
        else {
            panic!("operator root expected")
        };
        (exact, root)
    };
    let row = |k: &str, v: Option<f64>| {
        vec![
            ("k", Value::Utf8(k.into())),
            ("v", v.map_or(Value::Null, Value::Float64)),
            ("n", Value::Float64(0.)),
        ]
    };
    let rows = vec![row("a", None), row("a", Some(1.)), row("b", None)];
    assert_eq!(execute(&root, &rows), execute(&exact, &rows));
}

/// `count(*)` with no column known to be non-null (one nullable numeric
/// column, or several and none named `value`) is unsupported for every
/// candidate, not a count of a nullable column or an invalid DAG.
#[tokio::test]
async fn sql_count_star_without_a_non_null_column_is_unsupported() {
    for columns in [&[("v", true)][..], &[("a", true), ("b", true)][..]] {
        let catalog = table(columns);
        for (candidate, composed) in
            every_candidate(&catalog, "SELECT k, count(*) FROM t GROUP BY k").await
        {
            match composed {
                Err(asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError::Unsupported(_)) => {}
                other => panic!("{columns:?} {candidate:?}: {other:?}"),
            }
        }
    }
}

/// `count(v)` of a nullable `v` updates on `v` under its `v IS NOT NULL`
/// filter: it counts the non-null values. The runtime executes no filtered
/// state yet, so the shape is checked.
#[tokio::test]
async fn sql_count_of_a_nullable_column_counts_its_values() {
    let catalog = table(&[("v", true), ("n", false)]);
    for (candidate, composed) in
        every_candidate(&catalog, "SELECT k, count(v) FROM t GROUP BY k").await
    {
        let composed = composed.unwrap_or_else(|e| panic!("{candidate:?}: {e}"));
        composed[0].1.validate_structure().unwrap();
        let updates = count_updates(&composed[0].1);
        assert_eq!(updates.len(), 1, "{candidate:?}");
        assert!(reads_only(&updates[0], "v"), "{candidate:?}: {updates:?}");
    }
}
