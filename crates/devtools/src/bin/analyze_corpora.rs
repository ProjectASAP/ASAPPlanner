// cargo run -p asap-devtools --bin analyze_corpora -- --corpora --out-dir artifacts/promql_pre_asap
// cargo run -p asap-devtools --bin analyze_corpora -- --sql-corpora --out-dir artifacts/sql_pre_asap
//
// Corpus mode dumps all four PromQL corpora as JSONL and writes a heuristic
// anomaly report. The default mode remains the ad-hoc SQL/PromQL inspector.

use asap_devtools::{lower_promql, SqlCatalog};
use asap_frontend_sql::lower_sql_dialect;
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const DOCS: &str =
    include_str!("../../../frontend-promql/tests/observability/data/promql_corpus_docs.txt");
const TESTDATA: &str =
    include_str!("../../../frontend-promql/tests/observability/data/promql_corpus_testdata.txt");
const O11Y: &str =
    include_str!("../../../frontend-promql/tests/observability/data/o11y_bench_promql.txt");
const ALERTS: &str =
    include_str!("../../../frontend-promql/tests/observability/data/awesome_prometheus_alerts.txt");
const DQC: &str = include_str!(
    "../../../frontend-sql/tests/data_quality_check/data/synthetic_packet_trace_queries.sql"
);
const NETFLOW: &str = include_str!("../../../frontend-sql/tests/netflow/data/netflow.sql");
const BGP: &str = include_str!("../../../frontend-sql/tests/bgp_analytics/data/bgp_analytics.sql");
const BGP_WORKLOAD: &str = include_str!("../../../frontend-sql/tests/bgp_jan2024_workload/data/bgp_jan2024_rrc00_200_query_workload.yaml");

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
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
        col("origin", DataType::Utf8),
        col("next_hop", DataType::Utf8),
        col("local_pref", DataType::Utf8),
        col("med", DataType::Utf8),
        col("communities", DataType::Utf8),
        col("atomic", DataType::Utf8),
        col("aggr_asn", DataType::Utf8),
        col("aggr_ip", DataType::Utf8),
        col("source_file", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates.clone())
        .with_table("bgp.bgp_updates", updates)
}

fn sql_statements(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(serde::Deserialize)]
struct Workload {
    queries: Vec<WorkloadQuery>,
}

#[derive(serde::Deserialize)]
struct WorkloadQuery {
    sql: String,
}

fn corpus_lines(corpus: &str) -> Vec<&str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

#[derive(Serialize)]
struct DumpRecord {
    corpus: String,
    query_number: usize,
    expression: String,
    normalized_expression: String,
    structural_shape: String,
    lowered: bool,
    ir: Option<Value>,
    /// Lossless human-readable representation. JSON turns non-finite f64
    /// values such as NaN and infinities into null.
    ir_debug: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
struct CorpusResult {
    lowered: Vec<DumpRecord>,
    failed: Vec<DumpRecord>,
}

fn normalize(expression: &str) -> String {
    expression
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

// Coarse syntax grouping for triage; this is intentionally not semantic
// equivalence. Identifiers and literals become placeholders, while operators
// and punctuation remain visible.
fn structural_shape(expression: &str) -> String {
    let chars: Vec<char> = expression.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let quote = c;
            out.push_str("<str>");
            i += 1;
            while i < chars.len() {
                let escaped = chars[i] == '\\';
                let done = chars[i] == quote && !escaped;
                i += 1;
                if done {
                    break;
                }
                if escaped && i < chars.len() {
                    i += 1;
                }
            }
        } else if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(|x| x.is_ascii_digit()))
        {
            out.push_str("<num>");
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || ".+-".contains(chars[i]))
            {
                i += 1;
            }
        } else if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
            out.push_str("<id>");
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || "_:.".contains(chars[i]))
            {
                i += 1;
            }
        } else if !c.is_whitespace() {
            out.push(c);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

fn run_corpus(name: &str, source: &str) -> CorpusResult {
    let mut result = CorpusResult::default();
    for (index, expression) in corpus_lines(source).into_iter().enumerate() {
        let normalized_expression = normalize(expression);
        let structural_shape = structural_shape(expression);
        match lower_promql(expression, AccuracyTarget::Exact) {
            Ok(ir) => result.lowered.push(DumpRecord {
                corpus: name.to_string(),
                query_number: index + 1,
                expression: expression.to_string(),
                normalized_expression,
                structural_shape,
                lowered: true,
                ir: Some(serde_json::to_value(&ir).expect("QueryExpr must serialize")),
                ir_debug: Some(format!("{ir:#?}")),
                error: None,
            }),
            Err(error) => result.failed.push(DumpRecord {
                corpus: name.to_string(),
                query_number: index + 1,
                expression: expression.to_string(),
                normalized_expression,
                structural_shape,
                lowered: false,
                ir: None,
                ir_debug: None,
                error: Some(error.to_string()),
            }),
        }
    }
    result
}

fn write_jsonl(path: &Path, records: &[DumpRecord]) {
    let mut output = String::new();
    for record in records {
        output.push_str(&serde_json::to_string(record).expect("record must serialize"));
        output.push('\n');
    }
    std::fs::write(path, output)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

fn write_individual_dumps(dir: &Path, records: &[DumpRecord]) {
    std::fs::create_dir_all(dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    for record in records {
        let path = dir.join(format!("q{:04}.json", record.query_number));
        let contents = serde_json::to_vec_pretty(record).expect("record must serialize");
        std::fs::write(&path, contents)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }
}

fn examples(records: &[&DumpRecord]) -> String {
    records
        .iter()
        .take(4)
        .map(|r| format!("`{}`", r.expression))
        .collect::<Vec<_>>()
        .join("; ")
}

fn anomaly_report(all: &[DumpRecord], language: &str, manual_notes: &str) -> String {
    let successful: Vec<&DumpRecord> = all.iter().filter(|r| r.lowered).collect();
    let mut exact: BTreeMap<String, BTreeMap<String, Vec<&DumpRecord>>> = BTreeMap::new();
    let mut normalized: BTreeMap<String, BTreeMap<String, Vec<&DumpRecord>>> = BTreeMap::new();
    let mut shapes: BTreeMap<String, BTreeMap<String, Vec<&DumpRecord>>> = BTreeMap::new();
    let mut irs: BTreeMap<String, Vec<&DumpRecord>> = BTreeMap::new();
    for record in successful {
        // JSON maps non-finite f64 values to null, so use the Debug form for
        // fingerprints to avoid false collisions.
        let fingerprint = record.ir_debug.as_ref().unwrap().clone();
        exact
            .entry(record.expression.clone())
            .or_default()
            .entry(fingerprint.clone())
            .or_default()
            .push(record);
        normalized
            .entry(record.normalized_expression.clone())
            .or_default()
            .entry(fingerprint.clone())
            .or_default()
            .push(record);
        shapes
            .entry(record.structural_shape.clone())
            .or_default()
            .entry(fingerprint.clone())
            .or_default()
            .push(record);
        irs.entry(fingerprint).or_default().push(record);
    }
    let mut report = format!("# {language} pre-ASAP IR anomaly report\n\nThis is heuristic triage over successfully lowered queries only; findings require semantic review.\n\n");
    report.push_str("## Exact duplicate expressions with different IR\n\n");
    let mut found = 0;
    for (expression, groups) in exact.iter().filter(|(_, groups)| groups.len() > 1) {
        found += 1;
        report.push_str(&format!(
            "- `{expression}` → {} IR fingerprints\n",
            groups.len()
        ));
    }
    if found == 0 {
        report.push_str("None found.\n");
    }
    report.push_str("\n## Normalized expressions with different IR\n\n");
    found = 0;
    for (expression, groups) in normalized
        .iter()
        .filter(|(_, groups)| groups.len() > 1)
        .take(100)
    {
        found += 1;
        let records: Vec<&DumpRecord> = groups.values().flatten().copied().collect();
        report.push_str(&format!(
            "- `{expression}` → {} IR fingerprints; {}\n",
            groups.len(),
            examples(&records)
        ));
    }
    if found == 0 {
        report.push_str("None found.\n");
    }
    report.push_str("\n## Similar structural shapes with different IR\n\n");
    found = 0;
    for (shape, groups) in shapes
        .iter()
        .filter(|(_, groups)| groups.len() > 1)
        .take(100)
    {
        found += 1;
        let records: Vec<&DumpRecord> = groups.values().flatten().copied().collect();
        report.push_str(&format!(
            "- `{shape}` → {} IR fingerprints; {}\n",
            groups.len(),
            examples(&records)
        ));
    }
    if found == 0 {
        report.push_str("None found.\n");
    }
    report.push_str("\n## Distinct expressions with identical IR\n\n");
    found = 0;
    for records in irs
        .values()
        .filter(|records| {
            records
                .iter()
                .map(|r| &r.expression)
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        })
        .take(100)
    {
        found += 1;
        let distinct: BTreeSet<&str> = records.iter().map(|r| r.expression.as_str()).collect();
        report.push_str(&format!(
            "- {} expressions → one IR; {}\n",
            distinct.len(),
            distinct
                .iter()
                .take(4)
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if found == 0 {
        report.push_str("None found.\n");
    }

    report.push_str("\n## Manual review findings\n\n");
    report.push_str("- **Serialization caveat:** JSON renders non-finite floating-point values such as `NaN` and `±Inf` as `null`; use the `ir_debug` field in each per-query file when reviewing those values. Fingerprints use this lossless debug representation.\n");
    if language == "PromQL" {
        report.push_str("- **Known semantic collapse to review:** `rate(cumulative[5m])` and `");
        report
            .push_str("`irate(cumulative[5m])` both produce `AggIntent::Rate`. Confirm that this ");
        report.push_str(
            "abstraction is intentional; PromQL defines different sampling behavior for ",
        );
        report.push_str("the two functions.\n");
        report.push_str("- **Likely information-loss candidate:** `left_vector == bool fill(30) right_vector` and `left_vector == fill(30) right_vector` produce identical `BinaryOp` IR. PromQL's `bool` comparison modifier changes filtering into a 0/1-valued result, but the current `BinaryOp` representation has no return-bool field.\n");
        report.push_str("- **Apparently intentional canonicalization:** redundant parentheses, ");
        report.push_str(
            "duration spellings such as `50`/`50s`, and equivalent `@`/`offset` modifier ",
        );
        report.push_str("orderings produce identical IR.\n");
    }
    report.push_str(manual_notes);
    report
}

fn run_corpora(out_dir: PathBuf) {
    std::fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", out_dir.display()));
    let corpora = [
        ("docs", DOCS),
        ("testdata", TESTDATA),
        ("o11y_bench", O11Y),
        ("awesome_alerts", ALERTS),
    ];
    let mut all = Vec::new();
    let mut summary = Vec::new();
    for (name, source) in corpora {
        let mut result = run_corpus(name, source);
        let total = result.lowered.len() + result.failed.len();
        write_jsonl(&out_dir.join(format!("{name}.jsonl")), &result.lowered);
        write_jsonl(
            &out_dir.join(format!("{name}.errors.jsonl")),
            &result.failed,
        );
        write_individual_dumps(&out_dir.join(name), &result.lowered);
        write_individual_dumps(&out_dir.join(format!("{name}.errors")), &result.failed);
        summary.push(serde_json::json!({"corpus": name, "total": total, "lowered": result.lowered.len(), "failed": result.failed.len()}));
        all.append(&mut result.lowered);
        all.append(&mut result.failed);
    }
    std::fs::write(
        out_dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .expect("failed to write summary.json");
    std::fs::write(
        out_dir.join("anomalies.md"),
        anomaly_report(&all, "PromQL", ""),
    )
    .expect("failed to write anomalies.md");
    eprintln!(
        "wrote PromQL pre-ASAP dumps and anomaly report to {}",
        out_dir.display()
    );
}

async fn run_sql_corpora(out_dir: PathBuf) {
    std::fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", out_dir.display()));
    let workload: Workload =
        serde_yaml::from_str(BGP_WORKLOAD).expect("BGP workload YAML must parse");
    let corpora: Vec<(&str, Vec<String>, SqlCatalog, SqlDialect)> = vec![
        (
            "dqc_packet_trace",
            sql_statements(DQC),
            dqc_catalog(),
            SqlDialect::DataFusionSQL,
        ),
        (
            "netflow",
            sql_statements(NETFLOW),
            netflow_catalog(),
            SqlDialect::DataFusionSQL,
        ),
        (
            "bgp_analytics",
            sql_statements(BGP),
            bgp_catalog(),
            SqlDialect::ClickhouseSQL,
        ),
        (
            "bgp_jan2024_workload",
            workload.queries.into_iter().map(|q| q.sql).collect(),
            bgp_catalog(),
            SqlDialect::ClickhouseSQL,
        ),
    ];
    let mut all = Vec::new();
    let mut summary = Vec::new();
    for (name, queries, catalog, dialect) in corpora {
        let mut lowered = Vec::new();
        let mut failed = Vec::new();
        for (index, expression) in queries.into_iter().enumerate() {
            let normalized_expression = normalize(&expression);
            let structural_shape = structural_shape(&expression);
            match lower_sql_dialect(
                &expression,
                &catalog,
                dialect.clone(),
                AccuracyTarget::Exact,
            )
            .await
            {
                Ok(ir) => lowered.push(DumpRecord {
                    corpus: name.to_string(),
                    query_number: index + 1,
                    expression,
                    normalized_expression,
                    structural_shape,
                    lowered: true,
                    ir: Some(serde_json::to_value(&ir).expect("QueryExpr must serialize")),
                    ir_debug: Some(format!("{ir:#?}")),
                    error: None,
                }),
                Err(error) => failed.push(DumpRecord {
                    corpus: name.to_string(),
                    query_number: index + 1,
                    expression,
                    normalized_expression,
                    structural_shape,
                    lowered: false,
                    ir: None,
                    ir_debug: None,
                    error: Some(error.to_string()),
                }),
            }
        }
        let total = lowered.len() + failed.len();
        write_jsonl(&out_dir.join(format!("{name}.jsonl")), &lowered);
        write_jsonl(&out_dir.join(format!("{name}.errors.jsonl")), &failed);
        write_individual_dumps(&out_dir.join(name), &lowered);
        write_individual_dumps(&out_dir.join(format!("{name}.errors")), &failed);
        summary.push(serde_json::json!({
            "corpus": name,
            "total": total,
            "lowered": lowered.len(),
            "failed": failed.len()
        }));
        all.extend(lowered);
        all.extend(failed);
    }
    std::fs::write(
        out_dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .expect("failed to write summary.json");
    let notes = "- **Manual review result:** the reviewed SQL trees preserve `COUNT(*)` versus `COUNT(column)`, `COUNT(DISTINCT)`/`uniqExact`, `HAVING`, CTE-derived projections, `LAG`/`lagInFrame`, grouping keys, and explicit window frames. No concrete semantic collapse was found in this pass.\n- **Apparently intentional omission:** ClickHouse `FORMAT Null` is absent from the IR; it is an output/transport directive rather than query semantics.\n- **Failure boundaries:** unsupported ClickHouse functions and unsupported grammar are retained in the per-query error files rather than being converted into partial IR.\n";
    std::fs::write(
        out_dir.join("anomalies.md"),
        anomaly_report(&all, "SQL", notes),
    )
    .expect("failed to write anomalies.md");
    eprintln!(
        "wrote SQL pre-ASAP dumps and anomaly report to {}",
        out_dir.display()
    );
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--corpora") {
        let mut out_dir = PathBuf::from("artifacts/promql_pre_asap");
        while let Some(arg) = args.next() {
            if arg == "--out-dir" {
                out_dir = PathBuf::from(args.next().expect("--out-dir requires a path"));
            } else {
                panic!("unknown corpus-mode argument: {arg}");
            }
        }
        run_corpora(out_dir);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--sql-corpora") {
        let mut out_dir = PathBuf::from("artifacts/sql_pre_asap");
        let mut args = std::env::args().skip(2);
        while let Some(arg) = args.next() {
            if arg == "--out-dir" {
                out_dir = PathBuf::from(args.next().expect("--out-dir requires a path"));
            } else {
                panic!("unknown SQL corpus-mode argument: {arg}");
            }
        }
        run_sql_corpora(out_dir).await;
        return;
    }
    panic!("analyze_corpora requires --corpora or --sql-corpora");
}
