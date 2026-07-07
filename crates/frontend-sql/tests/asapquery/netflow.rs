//! Netflow SQL corpus used by the ASAPQuery benchmark path.
//!
//! This is a plain SQL corpus test for the netflow benchmark query families:
//! one aggregate over a netflow table, a time predicate, optional grouping,
//! optional `ORDER BY`/`LIMIT`, plus the nested aggregate shape.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
use asap_ir::intent_algebra::{AggIntent, GroupKeys, QueryExpr};
use asap_ir::types::AccuracyTarget;

const CORPUS: &str = include_str!("data/netflow.sql");

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "netflow_table",
        Schema::with_time_index(
            vec![
                col("time", DataType::Timestamp),
                col("srcip", DataType::Utf8),
                col("dstip", DataType::Utf8),
                col("srcport", DataType::Int64),
                col("dstport", DataType::Int64),
                col("proto", DataType::Utf8),
                col("pkt_len", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

fn queries() -> Vec<String> {
    let sql: String = CORPUS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--") && !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    sql.split(';')
        .map(|stmt| stmt.trim().to_string())
        .filter(|stmt| !stmt.is_empty())
        .collect()
}

#[derive(Clone, Copy)]
enum Expected {
    Quantile {
        q: f64,
        by: &'static [usize],
    },
    CountTopK {
        k: usize,
        inner_by: &'static [usize],
    },
    Aggregate {
        kind: AggKind,
        by: &'static [usize],
    },
    Nested {
        outer: AggKind,
        inner: AggKind,
    },
}

#[derive(Clone, Copy)]
enum AggKind {
    Sum,
    Max,
    Cardinality,
}

const EXPECTED: &[Expected] = &[
    Expected::Quantile { q: 0.95, by: &[1] },
    Expected::Quantile { q: 0.95, by: &[2] },
    Expected::CountTopK {
        k: 10,
        inner_by: &[1],
    },
    Expected::Aggregate {
        kind: AggKind::Sum,
        by: &[2],
    },
    Expected::Aggregate {
        kind: AggKind::Cardinality,
        by: &[1],
    },
    Expected::Aggregate {
        kind: AggKind::Max,
        by: &[1],
    },
    Expected::Nested {
        outer: AggKind::Max,
        inner: AggKind::Sum,
    },
];

#[tokio::test]
async fn asapquery_netflow_corpus_lowers_to_expected_intents() {
    let queries = queries();
    assert_eq!(
        queries.len(),
        EXPECTED.len(),
        "corpus fixture and expectation table drifted"
    );

    for (idx, (query, expected)) in queries.iter().zip(EXPECTED).enumerate() {
        let qe = lower_sql(query, &catalog(), AccuracyTarget::Exact)
            .await
            .unwrap_or_else(|err| panic!("q{} failed to lower:\n{query}\n{err}", idx + 1));
        qe.output_schema()
            .unwrap_or_else(|err| panic!("q{} schema derivation failed: {err}", idx + 1));
        assert!(
            has_scan_predicate(&qe),
            "q{} should retain the netflow time predicate on the Scan: {qe:?}",
            idx + 1
        );
        assert_expected(&qe, *expected, idx + 1);
    }
}

fn assert_expected(qe: &QueryExpr, expected: Expected, case_no: usize) {
    match expected {
        Expected::Quantile { q, by } => {
            let (actual_by, aggs) = first_aggregate(qe).expect("expected Aggregate");
            assert_eq!(
                actual_by,
                &GroupKeys::by(by.to_vec()),
                "q{case_no} GROUP BY"
            );
            assert!(
                aggs.iter()
                    .any(|agg| matches!(agg, AggIntent::Quantile { q: actual, .. } if (*actual - q).abs() < 1e-9)),
                "q{case_no} expected Quantile({q}), got {aggs:?}"
            );
        }
        Expected::CountTopK { k, inner_by } => {
            assert!(
                has_topk(qe, k),
                "q{case_no} expected count-ranked TopK({k}), got {qe:?}"
            );
            assert!(
                aggregate_by_with(qe, inner_by, |agg| matches!(agg, AggIntent::Count { .. })),
                "q{case_no} expected inner Count grouped by {inner_by:?}, got {qe:?}"
            );
        }
        Expected::Aggregate { kind, by } => {
            assert!(
                aggregate_by_with(qe, by, |agg| matches_kind(agg, kind)),
                "q{case_no} expected aggregate {} grouped by {by:?}, got {qe:?}",
                kind.name()
            );
        }
        Expected::Nested { outer, inner } => {
            let intents = all_intents(qe);
            assert!(
                intents.iter().any(|agg| matches_kind(agg, outer)),
                "q{case_no} expected outer {}, got {intents:?}",
                outer.name()
            );
            assert!(
                intents.iter().any(|agg| matches_kind(agg, inner)),
                "q{case_no} expected inner {}, got {intents:?}",
                inner.name()
            );
        }
    }
}

fn matches_kind(agg: &AggIntent, kind: AggKind) -> bool {
    matches!(
        (kind, agg),
        (AggKind::Sum, AggIntent::Sum { .. })
            | (AggKind::Max, AggIntent::Max { .. })
            | (AggKind::Cardinality, AggIntent::Cardinality { .. })
    )
}

impl AggKind {
    fn name(self) -> &'static str {
        match self {
            AggKind::Sum => "Sum",
            AggKind::Max => "Max",
            AggKind::Cardinality => "Cardinality",
        }
    }
}

fn first_aggregate(qe: &QueryExpr) -> Option<(&GroupKeys, &Vec<AggIntent>)> {
    match qe {
        QueryExpr::Aggregate { by, aggs, .. } => Some((by, aggs)),
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. } => first_aggregate(child),
        _ => None,
    }
}

fn has_scan_predicate(qe: &QueryExpr) -> bool {
    any_node(
        qe,
        |node| matches!(node, QueryExpr::Scan { predicates, .. } if !predicates.is_empty()),
    )
}

fn has_topk(qe: &QueryExpr, k: usize) -> bool {
    any_node(qe, |node| {
        matches!(
            node,
            QueryExpr::Aggregate { aggs, .. }
                if aggs.iter().any(|agg| matches!(agg, AggIntent::TopK { k: actual, .. } if *actual == k))
        )
    })
}

fn aggregate_by_with(
    qe: &QueryExpr,
    by: &'static [usize],
    pred: impl Fn(&AggIntent) -> bool,
) -> bool {
    let expected_by = GroupKeys::by(by.to_vec());
    let mut found = false;
    visit(qe, &mut |node| {
        if let QueryExpr::Aggregate { by, aggs, .. } = node {
            found |= *by == expected_by && aggs.iter().any(&pred);
        }
    });
    found
}

fn all_intents(qe: &QueryExpr) -> Vec<AggIntent> {
    let mut intents = Vec::new();
    visit(qe, &mut |node| {
        if let QueryExpr::Aggregate { aggs, .. } = node {
            intents.extend(aggs.iter().cloned());
        }
    });
    intents
}

fn any_node(qe: &QueryExpr, pred: impl Fn(&QueryExpr) -> bool) -> bool {
    let mut found = false;
    visit(qe, &mut |node| found |= pred(node));
    found
}

fn visit(qe: &QueryExpr, f: &mut impl FnMut(&QueryExpr)) {
    f(qe);
    match qe {
        QueryExpr::Project { child, .. }
        | QueryExpr::Filter { child, .. }
        | QueryExpr::Aggregate { child, .. }
        | QueryExpr::Window { child, .. }
        | QueryExpr::TimeRange { child, .. }
        | QueryExpr::Sort { child, .. }
        | QueryExpr::Limit { child, .. }
        | QueryExpr::Subquery { child, .. }
        | QueryExpr::Distinct { child, .. }
        | QueryExpr::WindowFunc { child, .. }
        | QueryExpr::Relabel { child, .. }
        | QueryExpr::Sample { child, .. }
        | QueryExpr::InfoJoin { child, .. } => visit(child, f),
        QueryExpr::BinaryOp { lhs, rhs, .. }
        | QueryExpr::Join {
            left: lhs,
            right: rhs,
            ..
        }
        | QueryExpr::SetOp {
            left: lhs,
            right: rhs,
            ..
        } => {
            visit(lhs, f);
            visit(rhs, f);
        }
        QueryExpr::Merge { children } => {
            for child in children {
                visit(child, f);
            }
        }
        QueryExpr::LetBinding { expr, child, .. } => {
            visit(expr, f);
            visit(child, f);
        }
        QueryExpr::VectorFromScalar(child) | QueryExpr::ScalarFromVector(child) => visit(child, f),
        QueryExpr::Scan { .. }
        | QueryExpr::Scalar(_)
        | QueryExpr::EvalTime
        | QueryExpr::Ref { .. } => {}
    }
}
