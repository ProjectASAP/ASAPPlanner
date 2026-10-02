// cargo run -p asap-lower --bin variant_coverage -- --data-ingestion-interval-ms 1000
//
// Lowers every query in every corpus we have (PromQL + SQL), walks the
// resulting `OperatorNode` DAGs, and reports which IR variants show up — per
// corpus, then rolled up globally: the operator vocabulary (`NonASAPOp` /
// `ASAPOp`, by `Operator::kind_name`) and the scalar-expression vocabulary
// (`ScalarExpr`) separately. Used to find the minimal IR node set.

use asap_devtools::lower_promql_with_data_ingestion_interval;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::ir::{OperatorNode, ScalarExpr};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use std::collections::BTreeSet;
use std::rc::Rc;

/// Every `Operator::kind_name()`: all `NonASAPOp` variants, then all `ASAPOp`
/// variants. A front end only ever emits the former; the latter are listed so
/// the "unused" report stays an honest view of the whole vocabulary.
const OPERATOR_VARIANTS: &[&str] = &[
    // NonASAPOp
    "Scan",
    "Values",
    "Filter",
    "Project",
    "Aggregate",
    "Join",
    "SetOp",
    "Concat",
    "Dedup",
    "Sort",
    "Limit",
    "BinaryOp",
    "SQLWindowFunc",
    "TimeRange",
    "TimeShift",
    "PromqlVectorFromScalar",
    "PromqlRelabel",
    "PromqlInfoEnrich",
    "PromqlSeriesSample",
    "PromqlSubquery",
    "ScalarBridge",
    // ASAPOp
    "SummaryAgg",
    "SummaryEstimate",
    "FinalizeExactAccumulator",
    "MaintainPopulation",
    "ReadPopulation",
    "SummaryMerge",
    "SummarySubtract",
    "SummaryDelete",
    "SummaryJoin",
    "Extension",
];

/// Every `ScalarExpr` variant, named as `scalar_kind_name` reports it.
const SCALAR_VARIANTS: &[&str] = &[
    "Column",
    "Literal",
    "Negative",
    "Compare",
    "BoolAnd",
    "BoolOr",
    "Not",
    "IsNull",
    "IsNotNull",
    "Cast",
    "InList",
    "FunctionCall",
    "Arithmetic",
    "Case",
    "CurrentTimestamp",
    "EvalTimestamp",
    "PromqlScalarFromVector",
    "ScalarSubquery",
    "Exists",
    "InSubquery",
];

/// The variant name of a scalar expression. Exhaustive on purpose: a new
/// `ScalarExpr` variant fails to compile here until it is named.
fn scalar_kind_name(e: &ScalarExpr) -> &'static str {
    use ScalarExpr::*;
    match e {
        Column(_) => "Column",
        Literal(_) => "Literal",
        Negative { .. } => "Negative",
        Compare { .. } => "Compare",
        BoolAnd(_) => "BoolAnd",
        BoolOr(_) => "BoolOr",
        Not(_) => "Not",
        IsNull(_) => "IsNull",
        IsNotNull(_) => "IsNotNull",
        Cast { .. } => "Cast",
        InList { .. } => "InList",
        FunctionCall { .. } => "FunctionCall",
        Arithmetic { .. } => "Arithmetic",
        Case { .. } => "Case",
        CurrentTimestamp => "CurrentTimestamp",
        EvalTimestamp => "EvalTimestamp",
        PromqlScalarFromVector(_) => "PromqlScalarFromVector",
        ScalarSubquery(_) => "ScalarSubquery",
        Exists { .. } => "Exists",
        InSubquery { .. } => "InSubquery",
    }
}

#[derive(Default)]
struct Variants {
    operators: BTreeSet<&'static str>,
    scalars: BTreeSet<&'static str>,
}

impl Variants {
    fn extend(&mut self, other: &Variants) {
        self.operators.extend(other.operators.iter().copied());
        self.scalars.extend(other.scalars.iter().copied());
    }
}

fn walk_scalar(e: &ScalarExpr, seen: &mut BTreeSet<&'static str>) {
    seen.insert(scalar_kind_name(e));
    for child in e.children() {
        walk_scalar(child, seen);
    }
}

/// Record every operator variant reachable from `root` (each shared node
/// once) and every scalar-expression variant owned by those operators. The
/// operator nodes a scalar expression reads (`scalar(v)`, subqueries) are in
/// `OperatorNode::children`, so `reachable` already covers them.
fn walk(root: &Rc<OperatorNode>, seen: &mut Variants) {
    for node in OperatorNode::reachable(root) {
        seen.operators.insert(node.operator.kind_name());
        if let Some(op) = node.non_asap() {
            for expr in op.scalar_exprs() {
                walk_scalar(expr, &mut seen.scalars);
            }
        }
    }
}

/// Line-based `#`/`--` comment stripping, then split on `;` — the shape every
/// SQL corpus test in this repo already uses.
fn sql_stmts(corpus: &str) -> Vec<String> {
    let sql: String = corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--") && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    sql.split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn promql_lines(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

fn dqc_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "packets",
        Schema::new(vec![
            col("srcip", DataType::Utf8),
            col("dstip", DataType::Utf8),
            col("srcport", DataType::Int64),
            col("dstport", DataType::Int64),
            col("proto", DataType::Utf8),
            col("time", DataType::Float64),
            col("pkt_len", DataType::Int64),
        ]),
    )
}

fn netflow_catalog() -> SqlCatalog {
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

fn bgp_catalog() -> SqlCatalog {
    let updates = Schema::new(vec![
        col("timestamp", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
        col("operation", DataType::Utf8),
        col("as_path", DataType::Utf8),
    ]);
    let rib = Schema::new(vec![
        col("snapshot_ts", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates.clone())
        .with_table("bgp.bgp_updates", updates)
        .with_table("bgp_rib_state", rib.clone())
        .with_table("bgp.bgp_rib_state", rib)
}

struct CorpusResult {
    name: &'static str,
    lowered: usize,
    failed: usize,
    variants: Variants,
}

fn report(r: &CorpusResult) {
    println!("--- {} ---", r.name);
    println!("lowered: {}, failed: {}", r.lowered, r.failed);
    println!(
        "operator variants ({}): {:?}",
        r.variants.operators.len(),
        r.variants.operators
    );
    println!(
        "scalar variants ({}): {:?}",
        r.variants.scalars.len(),
        r.variants.scalars
    );
    println!();
}

fn parse_ingestion_interval() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-ingestion-interval-ms" {
            return args
                .next()
                .expect("--data-ingestion-interval-ms requires a value")
                .parse()
                .expect("--data-ingestion-interval-ms must be an unsigned integer");
        }
    }
    panic!("--data-ingestion-interval-ms is required")
}

#[tokio::main]
async fn main() {
    let interval_ms = parse_ingestion_interval();
    let mut results = Vec::new();

    // ── PromQL corpora ──
    let promql_corpora: &[(&str, &str)] = &[
        (
            "promql/docs",
            include_str!(
                "../../../frontend-promql/tests/observability/data/promql_corpus_docs.txt"
            ),
        ),
        (
            "promql/testdata",
            include_str!(
                "../../../frontend-promql/tests/observability/data/promql_corpus_testdata.txt"
            ),
        ),
        (
            "promql/o11y_bench",
            include_str!("../../../frontend-promql/tests/observability/data/o11y_bench_promql.txt"),
        ),
        (
            "promql/awesome_alerts",
            include_str!(
                "../../../frontend-promql/tests/observability/data/awesome_prometheus_alerts.txt"
            ),
        ),
    ];
    for (name, corpus) in promql_corpora {
        let mut variants = Variants::default();
        let mut lowered = 0;
        let mut failed = 0;
        for q in promql_lines(corpus) {
            match lower_promql_with_data_ingestion_interval(q, AccuracyTarget::Exact, interval_ms) {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name,
            lowered,
            failed,
            variants,
        });
    }

    // ── SQL corpora ──
    #[allow(clippy::type_complexity)]
    let sql_corpora: &[(&str, &str, fn() -> SqlCatalog)] = &[
        ("sql/dqc_packet_trace", include_str!("../../../frontend-sql/tests/data_quality_check/data/synthetic_packet_trace_queries.sql"), dqc_catalog),
        ("sql/netflow", include_str!("../../../frontend-sql/tests/netflow/data/netflow.sql"), netflow_catalog),
    ];
    for (name, corpus, catalog_fn) in sql_corpora {
        let catalog = catalog_fn();
        let mut variants = Variants::default();
        let mut lowered = 0;
        let mut failed = 0;
        for q in sql_stmts(corpus) {
            match lower_sql_dialect(
                &q,
                &catalog,
                SqlDialect::DataFusionSQL,
                AccuracyTarget::Exact,
            )
            .await
            {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name,
            lowered,
            failed,
            variants,
        });
    }

    // bgp_analytics: ClickHouse dialect, mostly-rejected corpus (documented in
    // its own test) — still worth walking whatever *does* lower.
    {
        let corpus =
            include_str!("../../../frontend-sql/tests/bgp_analytics/data/bgp_analytics.sql");
        let catalog = bgp_catalog();
        let mut variants = Variants::default();
        let mut lowered = 0;
        let mut failed = 0;
        for q in sql_stmts(corpus) {
            match lower_sql_dialect(
                &q,
                &catalog,
                SqlDialect::ClickhouseSQL,
                AccuracyTarget::Exact,
            )
            .await
            {
                Ok(qe) => {
                    walk(&qe, &mut variants);
                    lowered += 1;
                }
                Err(_) => failed += 1,
            }
        }
        results.push(CorpusResult {
            name: "sql/bgp_analytics",
            lowered,
            failed,
            variants,
        });
    }

    for r in &results {
        report(r);
    }

    let mut global = Variants::default();
    let mut total_lowered = 0;
    let mut total_failed = 0;
    for r in &results {
        global.extend(&r.variants);
        total_lowered += r.lowered;
        total_failed += r.failed;
    }

    println!("=== global ===");
    println!("total lowered: {total_lowered}, total failed: {total_failed}\n");
    for (label, used, all) in [
        ("operator", &global.operators, OPERATOR_VARIANTS),
        ("scalar", &global.scalars, SCALAR_VARIANTS),
    ] {
        println!("used {label} variants ({}):", used.len());
        for v in used {
            println!("  {v}");
        }
        let unused: Vec<_> = all.iter().filter(|v| !used.contains(*v)).collect();
        println!("\nunused {label} variants ({}):", unused.len());
        for v in unused {
            println!("  {v}");
        }
        println!();
    }
}
