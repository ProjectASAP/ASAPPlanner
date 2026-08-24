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
//
// `--epsilon <f64>` is optional and applies to every query in the run: it
// lowers with `AccuracyTarget::Epsilon(<f64>)` instead of the default
// `AccuracyTarget::Exact`. Without it, every `AggIntent` lowers exact and
// `asap_aware_mapping::SketchAlgorithmStrategy` never has a genuine sketch
// alternative to report — so no node ever picks up a `SketchApproximation`
// note. Pass it to actually exercise that path, e.g.:
//   cargo run -p asap-lower --bin dag_export -- \
//       --epsilon 0.01 --sql "SELECT quantile(0.99, latency) FROM metrics" --name p99

use asap_types::dag_export::{self, DagGraph, DagNote, NamedGraph, WorkloadGraph};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

use asap_devtools::{lower_promql, lower_sql, SqlCatalog};

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
/// "<label>"` pairs off argv, in the order given, plus one optional global
/// `--epsilon <f64>`. `--name` is optional and applies to the immediately
/// preceding `--sql`/`--promql`.
fn parse_args() -> (Vec<(String, Lang, String)>, AccuracyTarget) {
    let mut entries: Vec<(String, Lang, String)> = Vec::new();
    let mut pending: Option<(Lang, String)> = None;
    let mut accuracy = AccuracyTarget::Exact;
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
            "--epsilon" => {
                let raw = args.next().expect("--epsilon requires a value");
                let epsilon: f64 = raw
                    .parse()
                    .unwrap_or_else(|_| panic!("--epsilon must be a float, got {raw:?}"));
                accuracy = AccuracyTarget::Epsilon(epsilon);
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }
    flush(&mut entries, &mut pending);
    (entries, accuracy)
}

/// Explain `qe` (issue #257's `asap_aware_mapping::explain_replacements`,
/// run as a single-query workload named `name`) and annotate every matching
/// [`DagGraph`] node — matched by [`asap_types::dag_export::DagNode::hash`]
/// equalling [`asap_aware_mapping::ReplacementExplanation::node_hash`] — with
/// a [`DagNote`]. Both sides compute `structural_hash` over the identical
/// `qe`, so a miss shouldn't happen; this is exploratory tooling, not a
/// load-bearing path, so a miss is logged and skipped rather than panicking.
fn annotate_with_explanations(graph: &mut DagGraph, name: &str, qe: &QueryExpr) {
    let explanations =
        asap_aware_mapping::explain_replacements(vec![(name.to_string(), qe.clone())]);
    for explanation in explanations {
        let mut matched = false;
        for node in graph.nodes.iter_mut() {
            if node.hash == explanation.node_hash {
                node.notes.push(DagNote {
                    kind: format!("{:?}", explanation.kind),
                    reason: explanation.reason.clone(),
                });
                matched = true;
            }
        }
        if !matched {
            eprintln!(
                "dag_export: explanation for {name:?} ({:?}, node_hash={}) matched no \
                 DagNode by hash — this shouldn't happen, since both sides hash the \
                 same QueryExpr",
                explanation.kind, explanation.node_hash
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let (entries, accuracy) = parse_args();
    if entries.is_empty() {
        eprintln!("usage: dag_export --sql \"<query>\" [--name <label>] [--epsilon <f64>] ...");
        std::process::exit(1);
    }

    let mut queries = Vec::new();
    for (name, lang, query) in entries {
        let lowered = match lang {
            Lang::Sql => lower_sql(&query, &catalog(), accuracy.clone())
                .await
                .map_err(|e| e.to_string()),
            Lang::PromQl => lower_promql(&query, accuracy.clone()).map_err(|e| e.to_string()),
        };
        match lowered {
            Ok(qe) => {
                let mut graph = dag_export::export(&qe);
                annotate_with_explanations(&mut graph, &name, &qe);
                queries.push(NamedGraph {
                    name,
                    source: Some(query),
                    graph,
                });
            }
            Err(e) => eprintln!("skipping {name:?} — lowering failed: {e}"),
        }
    }

    let workload = WorkloadGraph { queries };
    println!("{}", serde_json::to_string_pretty(&workload).unwrap());
}
