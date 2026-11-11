//! Frontends → Pass 1's memo: every candidate Pass 1 adds outputs its
//! group's schema (`Memo::add_candidate` checks it), so every way to compute
//! a query keeps its output columns (names, types and nullability).

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_stage1_pass1_per_computation_candidates::pass1_memo;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot, ScalarExpr};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{SketchAlgorithm, SummaryInputExpr, SummaryUpdate};
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

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
    let root = lower_sql(query, &catalog)
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
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1000),
        )),
    };
    // As the planner does before Stage 0 (`Stage1ToStage3Pass`): each
    // series' identity is realized as a column.
    match asap_frontend_promql::lower_promql_query_workload(&workload)
        .unwrap_or_else(|e| panic!("lower failed for {query:?}: {e}"))
        .remove(0)
    {
        QueryRoot::Operator(node) => QueryRoot::Operator(
            asap_types::ir::schema::series_identity::with_promql_series_identity(&node)
                .unwrap_or(node),
        ),
        scalar => scalar,
    }
}

/// Stage 0's memo of the one query `root`, without an accuracy target.
fn stage0(root: QueryRoot) -> Memo {
    asap_stage0_memo::memo_of_queries(&[root], &[None], &DataWorkload::default()).unwrap()
}

/// Pass 1's memo of the one query `root`.
fn pass1(root: QueryRoot) -> Memo {
    pass1_memo(stage0(root), None).unwrap()
}

/// The one `Aggregate` group of `memo`.
fn root_group(memo: &Memo) -> MemoGroupId {
    let [group] = aggregate_groups(memo)[..] else {
        panic!("one aggregate group expected")
    };
    group
}

/// Pass 1 over `root`; returns how many candidates its aggregate groups
/// have. Each outputs its group's schema, or `Memo::add_candidate` would
/// have rejected it and Pass 1 failed.
fn candidates_of_aggregates(label: &str, root: QueryRoot) -> usize {
    let memo = pass1_memo(stage0(root), None).unwrap_or_else(|e| panic!("{label}: {e}"));
    aggregate_groups(&memo)
        .iter()
        .map(|group| memo.groups()[group.0 as usize].candidates.len())
        .sum()
}

/// The groups of `memo` whose expression is an `Aggregate`.
fn aggregate_groups(memo: &Memo) -> Vec<MemoGroupId> {
    memo.groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            matches!(
                group.expression,
                Operator::NonASAP(NonASAPOp::Aggregate { .. })
            )
        })
        .map(|(index, _)| MemoGroupId(index as u32))
        .collect()
}

#[tokio::test]
async fn sql_candidates_output_the_query_columns() {
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
        assert!(candidates_of_aggregates(query, sql(query).await) > 1, "{query}");
    }
}

#[test]
fn promql_candidates_output_the_query_columns() {
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
        assert!(
            candidates_of_aggregates(query, promql(query)) > 1,
            "{query}"
        );
    }
}

/// The algorithm and update of every sketch state group of Pass 1's memo
/// of `root`.
fn sketch_builds(root: QueryRoot) -> Vec<(SketchAlgorithm, SummaryUpdate)> {
    pass1(root)
        .groups()
        .iter()
        .filter_map(|group| match &group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                summary_type: FieldDataType::Sketch(kind),
                input,
                ..
            }) => Some((*kind, input.clone())),
            _ => None,
        })
        .collect()
}

/// A PromQL top-k over per-series sums offers both heap algorithms, and a
/// count offers Count-Min.
#[test]
fn topk_offers_both_heap_algorithms() {
    let topk = sketch_builds(promql("topk by (job) (10, sum_over_time(m[1m]))"));
    let algorithms: Vec<_> = topk.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        algorithms,
        [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
    let count = sketch_builds(promql("count by (job) (m)"));
    assert!(count.iter().any(|(kind, _)| *kind == SketchAlgorithm::Cms));
}

/// Top-K over a grouped sum is a whole-expression heap: it absorbs the
/// sum and ranks its group keys, read from the rows the sum reads.
#[test]
fn topk_over_an_aggregate_ranks_its_labels() {
    let builds = sketch_builds(promql("topk(3, sum by (job) (rate(m[5m])))"));
    assert_eq!(builds.len(), 2);
    for (_, update) in &builds {
        assert_eq!(
            update.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("job".into())))
        );
    }
}

/// A subquery inside a measure filter is an input group of the filtered
/// state: the outer count's state reads the inner quantile's group, which
/// has its own candidates (exact and the quantile sketches' readouts).
#[tokio::test]
async fn subquery_in_a_measure_filter_is_an_input_group_of_the_state() {
    let query = "SELECT src, count(*) FILTER (WHERE bytes > (SELECT approx_percentile_cont(bytes, 0.5) FROM flows)) \
                 FROM flows GROUP BY src";
    let memo = pass1(sql(query).await);
    let subqueries: Vec<MemoGroupId> = memo
        .groups()
        .iter()
        .filter_map(|group| match &group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                filter: Some(filter),
                ..
            }) => Some(
                filter
                    .0
                    .operator_refs()
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(!subqueries.is_empty());
    // The subquery reads the inner quantile's aggregate group.
    for subquery in subqueries {
        let below = memo.groups()[subquery.0 as usize].expression.children();
        assert!(below.iter().any(|group| {
            matches!(
                memo.groups()[group.0 as usize].expression,
                Operator::NonASAP(NonASAPOp::Aggregate { .. })
            ) && memo.groups()[group.0 as usize].candidates.len() > 1
        }));
    }
}

/// The candidate of `memo`'s root group that is not its own expression
/// and computes `avg` from its `sum` and `count` states, and every group
/// below it.
fn avg_candidate_groups(memo: &Memo) -> Vec<MemoGroupId> {
    let root = root_group(memo);
    let group = &memo.groups()[root.0 as usize];
    let mut below: Vec<MemoGroupId> = Vec::new();
    let mut to_visit: Vec<MemoGroupId> = group.candidates[1..]
        .iter()
        .flat_map(|candidate| {
            candidate
                .expression
                .operator
                .children()
                .into_iter()
                .copied()
        })
        .collect();
    while let Some(next) = to_visit.pop() {
        if !below.contains(&next) {
            below.push(next);
            to_visit.extend(
                memo.groups()[next.0 as usize]
                    .expression
                    .children()
                    .into_iter()
                    .copied(),
            );
        }
    }
    below
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
        let exact = lower_sql(query, &catalog).await.unwrap();
        let memo = pass1(QueryRoot::Operator(exact));
        let root = &memo.groups()[root_group(&memo).0 as usize];
        assert!(
            root.group_properties.schema.fields.last().unwrap().nullable,
            "{query}"
        );
        assert!(root.candidates.len() > 1, "{query}");
        let null_when_no_count = memo.groups().iter().any(|group| {
            let Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) = &group.expression
            else {
                return false;
            };
            let count = memo.groups()[child.0 as usize]
                .group_properties
                .schema
                .fields
                .len()
                - 1;
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
        assert!(null_when_no_count, "{query}");
    }
}

/// PromQL `avg` divides its `sum` and `count` vectors with a `BinaryOp`,
/// grouped, global and per series alike. Prometheus averages incrementally
/// once a sum overflows, so the division stands in only for finite operands
/// and quotient (`checked_finite_division`).
#[test]
fn promql_avg_divides_sum_and_count_vectors() {
    for query in ["avg by (job) (m)", "avg(m)", "avg_over_time(m[5m])"] {
        let memo = pass1(promql(query));
        let root = &memo.groups()[root_group(&memo).0 as usize];
        assert!(root.candidates.len() > 1, "{query}");
        let kinds: Vec<_> = avg_candidate_groups(&memo)
            .iter()
            .map(|group| memo.groups()[group.0 as usize].expression.kind_name())
            .collect();
        assert!(!kinds.contains(&"Join"), "{query}: {kinds:?}");
        let divisions: Vec<bool> = memo
            .groups()
            .iter()
            .flat_map(|group| &group.candidates)
            .filter_map(|candidate| match &candidate.expression.operator {
                Operator::NonASAP(NonASAPOp::BinaryOp { operator, .. }) => {
                    Some(operator.checked_finite_division)
                }
                _ => None,
            })
            .collect();
        assert!(!divisions.is_empty(), "{query}");
        assert!(divisions.iter().all(|checked| *checked), "{query}");
    }
}

/// `count(*)` over `t(k, v)`, every column nullable, under a key-only
/// `HAVING`: no count accumulator or sketch can be built, so Pass 1 adds no
/// group, not even the `Filter` its candidates would push below the
/// aggregate.
#[tokio::test]
async fn an_unsupported_way_under_a_key_only_having_adds_no_group() {
    let root = lower_sql(
        "SELECT k, count(*) FROM t GROUP BY k HAVING k > 'a'",
        &table(&[("v", true)]),
    )
    .await
    .unwrap();
    let stage0 = stage0(QueryRoot::Operator(root));
    let memo = pass1_memo(stage0.clone(), None).unwrap();
    assert_eq!(memo.groups().len(), stage0.groups().len());
    assert!(memo
        .groups()
        .iter()
        .all(|group| !group.reached_by_queries().is_empty()));
}

/// A multi-measure aggregate's `count(*)` is the same group as another
/// query's `count(*)` over the same rows: Pass 1 writes the measure's own
/// aggregate as the frontend writes one.
#[tokio::test]
async fn a_multi_measure_aggregates_part_is_the_equal_stage0_group() {
    let roots = [
        sql("SELECT src, count(*), sum(bytes) FROM flows GROUP BY src").await,
        sql("SELECT src, count(*) FROM flows GROUP BY src").await,
    ];
    let stage0 =
        asap_stage0_memo::memo_of_queries(&roots, &[None, None], &DataWorkload::default()).unwrap();
    let memo = pass1_memo(stage0, None).unwrap();
    let counts = memo
        .groups()
        .iter()
        .filter(|group| {
            matches!(
                &group.expression,
                Operator::NonASAP(NonASAPOp::Aggregate { measures, .. })
                    if measures.as_slice() == [AggIntent::Count]
            )
        })
        .count();
    assert_eq!(counts, 1);
}

/// SQL `sum` of a subquery's `sum`, read through the subquery's renaming
/// `Project`: the outer sum's state also gets one exact sum over the raw
/// rows, a fused build listing the `Project` and the inner aggregate.
#[tokio::test]
async fn sql_sum_of_a_subquerys_sum_is_one_sum_over_the_rows() {
    let memo = pass1(
        sql("SELECT src, sum(s) FROM \
             (SELECT src, dst, sum(bytes) AS s FROM flows GROUP BY src, dst) x GROUP BY src")
        .await,
    );
    let fused: Vec<_> = memo
        .groups()
        .iter()
        .flat_map(|group| &group.candidates)
        .filter(|candidate| {
            matches!(
                candidate.expression.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { .. })
            ) && !candidate.fused_groups.is_empty()
        })
        .collect();
    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].fused_groups.len(), 2);
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

/// Pass 1's memo of `query` over `catalog`.
async fn pass1_sql(catalog: &SqlCatalog, query: &str) -> Memo {
    let root = lower_sql(query, catalog).await.unwrap();
    pass1(QueryRoot::Operator(root))
}

/// The update of every `count` state (exact or sketch) in `memo`.
fn count_updates(memo: &Memo) -> Vec<SummaryUpdate> {
    memo.groups()
        .iter()
        .filter_map(|group| match &group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                summary_type,
                input,
                ..
            }) if !matches!(
                summary_type,
                FieldDataType::ExactAggregate(asap_types::summary_type::ExactKind::Sum, _)
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
    let column = SummaryInputExpr::Column(ColumnRef::Named(name.into()));
    match &update.item {
        Some(item) => *item == column && update.weight == SummaryInputExpr::Constant(1.0),
        None => update.weight == column,
    }
}

/// SQL `avg` as `sum` and `count` keeps an explicit output alias: its
/// candidate outputs the root group's schema, which names it.
#[tokio::test]
async fn sql_avg_keeps_its_explicit_output_name() {
    let memo = pass1_sql(&avg_table(), "SELECT avg(value) AS avg_latency FROM t").await;
    assert!(memo.groups()[root_group(&memo).0 as usize].candidates.len() > 1);
    let QueryRoot::Operator(query_root) = &memo.queries()[0].root else {
        panic!("operator root expected")
    };
    let output = &memo.groups()[query_root.0 as usize].group_properties.schema;
    assert_eq!(output.fields.last().unwrap().name, "avg_latency");
}

/// `count(*)` with no column known to be non-null (one nullable numeric
/// column, or several and none named `value`) gets no accumulator or
/// sketch: the root group keeps only its plain exact aggregate, not a count
/// of a nullable column.
#[tokio::test]
async fn sql_count_star_without_a_non_null_column_is_only_exact() {
    for columns in [&[("v", true)][..], &[("a", true), ("b", true)][..]] {
        let memo = pass1_sql(&table(columns), "SELECT k, count(*) FROM t GROUP BY k").await;
        assert_eq!(
            memo.groups()[root_group(&memo).0 as usize].candidates.len(),
            1,
            "{columns:?}"
        );
        assert!(count_updates(&memo).is_empty(), "{columns:?}");
    }
}

/// `count(v)` of a nullable `v` updates on `v` under its `v IS NOT NULL`
/// filter: it counts the non-null values. The runtime executes no filtered
/// state yet, so the shape is checked.
#[tokio::test]
async fn sql_count_of_a_nullable_column_counts_its_values() {
    let catalog = table(&[("v", true), ("n", false)]);
    let memo = pass1_sql(&catalog, "SELECT k, count(v) FROM t GROUP BY k").await;
    let updates = count_updates(&memo);
    assert!(!updates.is_empty());
    for update in &updates {
        assert!(reads_only(update, "v"), "{updates:?}");
    }
}
