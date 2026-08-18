// cargo run -p asap-lower --bin dag_export -- \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
//     --promql "topk(5, rate(http_requests_total[5m]))" --name q2
//
// Lowers each given SQL/PromQL query to pre-ASAP IR and prints a single
// `asap_types::dag_export::WorkloadGraph` as JSON on stdout — the input format
// for `tools/dag-viewer` (issue #133). Redirect to a file and load it there:
//   cargo run -p asap-lower --bin dag_export -- --sql "..." --name q1 > /tmp/dag.json
//
// `--name` is optional; an unnamed query defaults to `q<n>` (1-indexed).

use asap_devtools::{lower_promql, lower_sql, SqlCatalog};
use asap_types::dag_export::{self, NamedGraph, WorkloadGraph};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

enum Lang {
    Sql,
    PromQl,
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
            "metrics",
            Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("region", DataType::Utf8, false),
                    Column::new("latency", DataType::Float64, false),
                    Column::new("bytes", DataType::Int64, false),
                ],
                0,
                vec![],
            ),
        )
        .with_table(
            "hosts",
            Schema::new(vec![
                Column::new("service", DataType::Utf8, false),
                Column::new("region", DataType::Utf8, false),
            ]),
        )
}

/// Parses `--sql "<query>" --name "<label>"` / `--promql "<query>" --name
/// "<label>"` pairs off argv, in the order given. `--name` is optional and
/// applies to the immediately preceding `--sql`/`--promql`.
fn parse_args() -> Vec<(String, Lang, String)> {
    let mut entries: Vec<(String, Lang, String)> = Vec::new();
    let mut pending: Option<(Lang, String)> = None;
    let mut args = std::env::args().skip(1);

    fn flush(entries: &mut Vec<(String, Lang, String)>, pending: &mut Option<(Lang, String)>) {
        if let Some((lang, query)) = pending.take() {
            entries.push((format!("q{}", entries.len() + 1), lang, query));
        }
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sql" | "--promql" => {
                flush(&mut entries, &mut pending);
                let query = args
                    .next()
                    .unwrap_or_else(|| panic!("{arg} requires a query string"));
                let lang = if arg == "--sql" {
                    Lang::Sql
                } else {
                    Lang::PromQl
                };
                pending = Some((lang, query));
            }
            "--name" => {
                let name = args.next().expect("--name requires a value");
                let (lang, query) = pending.take().expect("--name must follow --sql/--promql");
                entries.push((name, lang, query));
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }
    flush(&mut entries, &mut pending);
    entries
}

#[tokio::main]
async fn main() {
    let entries = parse_args();
    if entries.is_empty() {
        eprintln!("usage: dag_export --sql \"<query>\" [--name <label>] ...");
        std::process::exit(1);
    }

    let mut queries = Vec::new();
    for (name, lang, query) in entries {
        let graph = match lang {
            Lang::Sql => lower_sql(&query, &catalog(), AccuracyTarget::Exact)
                .await
                .map(|qe| dag_export::export(&qe))
                .map_err(|e| e.to_string()),
            Lang::PromQl => lower_promql(&query, AccuracyTarget::Exact)
                .map(|qe| dag_export::export(&qe))
                .map_err(|e| e.to_string()),
        };
        match graph {
            Ok(graph) => queries.push(NamedGraph {
                name,
                source: Some(query),
                graph,
            }),
            Err(e) => eprintln!("skipping {name:?} — lowering failed: {e}"),
        }
    }

    let workload = WorkloadGraph { queries };
    println!("{}", serde_json::to_string_pretty(&workload).unwrap());
}
