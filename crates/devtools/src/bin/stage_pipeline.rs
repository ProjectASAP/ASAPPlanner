// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --example planner-layering-1 --out planner-layering-example1.json
// (also planner-layering-2: #509 Example 2, three SQL flow statistics;
// planner-layering-3a and planner-layering-3b: #509 Example 3, Patterns A
// and B; 3a has 972 candidates, all priced, 64 shown by default)
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --promql "topk by (job) (10, rate(x[1m]))" --epsilon 0.01 --delta 0.001 --out run.json
// cargo run -p asap-devtools --bin stage_pipeline -- \
//     --table '{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"},
//               {"name": "service", "type": "utf8"}], "time_index": 0}' \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --out run.json
//
// Writes an `asap-stage-pipeline/v1` document (tools/dag-viewer) with the
// four planner stages (#509 MVP):
//   - stage0_query_language_frontends: the frontends' workload DAG, one root per query;
//   - stage1_logical_asap_optimization: Pass 1 whole-workload candidates, one per
//     choice of a per-computation candidate for every target (the Cartesian
//     product), each followed by its Pass 2 candidates (the
//     identical-expression rule's, one per subset of its sharing groups,
//     then the summary-capability rule's, one per choice of shared summary
//     builds), in enumeration order. Every candidate is optimized and
//     priced; the document shows the first `--max-displayed` (default 64)
//     and the selected one, with `combinations` (how many exist) and
//     `capped` (whether some are not shown). A candidate that cannot be
//     composed carries `error` instead of `dag`;
//   - stage2_physical_asap_optimization: one physical candidate per composed logical
//     candidate (operator implementation; every sub-DAG NotMaterialized), no
//     cost; a composed logical candidate Stage 2 cannot implement is listed
//     under `pruned` with the reason;
//   - stage3_plan_selection: the selected candidate, chosen among all candidates;
//     the shown candidates' costs and accepted accuracy (per query: the
//     target's metric or null when not set, the accepted guarantee's
//     metric or null when exact, the sketches read and a one-line
//     summary), and every other shown candidate as
//     rejected (`valid: false`) or costlier. Each node's `cost` is one
//     computation's price times `multiplier`, the times it is computed:
//     once if materialized, else once per query (or materialized node)
//     that reads it, so a shared NotMaterialized node is charged once per
//     query. When selection fails, `selected` is null, with the rejections,
//     or with `error` for any other failure.
// When Pass 1 fails, the document still has the Stage 0 DAG; Stage 1 has
// `error` and no candidates, and nothing is selected.
//
// A workload with more than `--max-combinations` (default 10^4) Stage 1
// candidates, Pass 2's included, is refused as soon as enumeration passes
// the limit, and so is one whose Stage 2 physical candidates (one per
// materialization assignment, counted before they are built) pass it: the
// run exits with status 3 and the limit, and writes no document.
// Planning never caps or truncates the candidates it ranks; only the display
// does (`--max-displayed`).
//
// `--promql` and `--sql` may repeat and mix, in workload order; `--name`
// names the query before it (default `q<n>`). `--epsilon`/
// `--delta` apply to every query; without them the queries are exact.
// `--metric` (`absolute`, `absolute_stream_l1`, `absolute_stream_l2`,
// `relative` or `rank`) sets the error metric of that target; without it,
// a candidate is accepted in whatever metric its guarantee meets ε (and δ)
// in, and `stage3_plan_selection.accepted` reports, per shown valid
// candidate and query, the metric accepted (e.g. "metric not set; accepted
// rank (Kll)").
// `--interval-ms` is the source cadence (default 15000). `--table '<json>'`
// (repeatable) declares a table the SQL queries read: `{"name": ...,
// "columns": [{"name": ..., "type": "timestamp|utf8|string|float64|double|
// int64|bigint", "nullable": true}], "time_index": 0}` (`nullable` defaults
// to true; without `time_index` the table has no time column; a
// `time_index` that is not a column index is an error).

use std::collections::HashSet;

use asap_aware_mapping::stage3_plan_selection::{Rejection, Selection, SelectionError};
use asap_devtools::candidate_labels::candidate_labels;
use asap_devtools::planner_layering_examples::{
    declared, example1, example2, example2_catalog, pattern_a, pattern_b, PatternARecurrence,
};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_planning_stages, run_planning_stages_over,
    PlanningStagesError, PlanningStagesRun,
};
use asap_devtools::SqlCatalog;
use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::QueryRoot;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::{default_metric_for_root, AccuracyTarget, ErrorMetric};
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, PlanningWorkload, Query, QueryLanguage,
    QueryRecurrence, QueryRequirements, QueryWorkload, RepeatedDemand, SqlDialect,
};
use serde_json::{json, Value};

const USAGE: &str =
    "usage: stage_pipeline (--example planner-layering-{1,2,3a,3b} | ((--promql | --sql) <query> \
[--name <name>]... [--table <json>...] [--epsilon <f64> --delta <f64> --metric <metric>] [--interval-ms <u64>])) \
[--max-combinations <n>] [--max-displayed <n>] --out <file>";

/// The default `--max-combinations`: the most Stage 1 candidates (Pass 2's
/// included) a run enumerates.
const MAX_COMBINATIONS: usize = 10_000;

/// Why a run wrote no document.
enum Failure {
    /// Bad arguments or a query that does not plan (exit status 2).
    Usage(String),
    /// More Stage 1 candidates than `--max-combinations` (exit status 3).
    TooManyCandidates(PlanningStagesError),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Usage(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Failure::Usage(message.into())
    }
}

fn main() {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => {}
        Err(Failure::Usage(message)) => {
            eprintln!("stage_pipeline: {message}\n{USAGE}");
            std::process::exit(2);
        }
        Err(Failure::TooManyCandidates(error)) => {
            eprintln!("stage_pipeline: {error}");
            std::process::exit(3);
        }
    }
}

/// The document for a run that ended with `result` over the frontends'
/// `roots`. A Pass 1 failure still gives a document: the Stage 0 DAG, the
/// error under Stage 1 and no selection. Refusing too many combinations or
/// a failed validation gives none.
fn document_of(
    queries: Vec<Value>,
    roots: &[QueryRoot],
    result: Result<PlanningStagesRun, PlanningStagesError>,
    max_displayed: usize,
) -> Result<Value, Failure> {
    let queries = with_default_metrics(queries, roots);
    match result {
        Ok(run) => Ok(stage_pipeline(queries, &run, max_displayed)),
        Err(PlanningStagesError::Pass1(error)) => Ok(json!({
            "format": "asap-stage-pipeline/v1",
            "workload": { "queries": queries },
            "stage0_query_language_frontends": { "dag": export(roots) },
            "stage1_logical_asap_optimization": {
                "combinations": 0,
                "capped": false,
                "candidates": [],
                "error": format!("Pass 1: {error}"),
            },
            "stage2_physical_asap_optimization": { "candidates": [], "pruned": [] },
            "stage3_plan_selection": { "costs": {}, "selected": null, "rejected": [] },
        })),
        Err(
            error @ (PlanningStagesError::TooManyCandidates { .. }
            | PlanningStagesError::TooManyPhysicalCandidates { .. }),
        ) => Err(Failure::TooManyCandidates(error)),
        Err(error @ PlanningStagesError::Validation(_)) => Err(error.to_string().into()),
    }
}

fn run(args: Vec<String>) -> Result<(), Failure> {
    let mut example = None;
    // (language, text, name) in workload order.
    let mut queries: Vec<(QueryLanguage, String, Option<String>)> = Vec::new();
    let mut tables = Vec::new();
    let (mut epsilon, mut delta, mut interval_ms) = (None, None, 15_000u64);
    let mut metric: Option<ErrorMetric> = None;
    let mut max_displayed = 64usize;
    let mut max_combinations = MAX_COMBINATIONS;
    let mut out = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{v}: {e}"));
        match flag.as_str() {
            "--example" => example = Some(value()?),
            "--promql" => queries.push((QueryLanguage::PromQL, value()?, None)),
            "--sql" => queries.push((
                QueryLanguage::SQL(SqlDialect::DataFusionSQL),
                value()?,
                None,
            )),
            "--name" => {
                let name = value()?;
                match queries.last_mut() {
                    Some((_, _, slot @ None)) => *slot = Some(name),
                    _ => return Err("--name follows a --promql or --sql query".into()),
                }
            }
            "--table" => tables.push(value()?),
            "--epsilon" => epsilon = Some(number(value()?)?),
            "--delta" => delta = Some(number(value()?)?),
            "--metric" => {
                let name = value()?;
                metric = Some(
                    serde_json::from_value(Value::String(name.clone()))
                        .map_err(|_| format!("unknown metric {name}"))?,
                )
            }
            "--interval-ms" => interval_ms = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-displayed" => max_displayed = value()?.parse().map_err(|e| format!("{e}"))?,
            "--max-combinations" => {
                max_combinations = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--out" => out = Some(value()?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let has_sql = queries
        .iter()
        .any(|(language, _, _)| *language != QueryLanguage::PromQL);
    if !tables.is_empty() && !has_sql {
        return Err("--table needs --sql".into());
    }
    let out = out.ok_or("--out is required")?;
    let document = match (example.as_deref(), queries.is_empty()) {
        (Some(example), true) => {
            let workload = match example {
                "planner-layering-1" => example1(),
                "planner-layering-2" => example2(),
                "planner-layering-3a" => {
                    pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed)
                }
                "planner-layering-3b" => pattern_b(),
                other => return Err(format!("unknown example {other}").into()),
            };
            let roots = match workload.query_workload.language {
                QueryLanguage::PromQL => lower_promql_roots(&workload)?,
                _ => lower_sql_roots(&workload, &example2_catalog())?,
            };
            let result = run_planning_stages(&workload, roots.clone(), Some(max_combinations));
            document_of(workload_queries(&workload), &roots, result, max_displayed)?
        }
        (None, false) => {
            let accuracy = match (epsilon, delta) {
                (None, None) => AccuracyTarget::exact(),
                (Some(epsilon), None) => AccuracyTarget::epsilon(epsilon),
                (Some(epsilon), Some(delta)) => AccuracyTarget::epsilon_delta(epsilon, delta),
                (None, Some(_)) => return Err("--delta needs --epsilon".into()),
            };
            let accuracy = match metric {
                Some(_) if epsilon.is_none() => return Err("--metric needs --epsilon".into()),
                Some(metric) => accuracy.with_metric(metric),
                None => accuracy,
            };
            let catalog = sql_catalog(&tables)?;
            let mut names = HashSet::new();
            let (mut described, mut roots, mut entries) = (Vec::new(), Vec::new(), Vec::new());
            for (index, (language, text, name)) in queries.into_iter().enumerate() {
                let (name, defaulted) = match name {
                    Some(name) => (name, false),
                    None => (format!("q{}", index + 1), true),
                };
                if !names.insert(name.clone()) {
                    return Err(if defaulted {
                        format!(
                            "duplicate query name {name} (the default name of query {})",
                            index + 1
                        )
                    } else {
                        format!("duplicate query name {name}")
                    }
                    .into());
                }
                // Each query lowers in its own language; planning sees one workload.
                let one = query_batch(language.clone(), &[text], accuracy.clone(), interval_ms);
                roots.extend(match language {
                    QueryLanguage::PromQL => lower_promql_roots(&one)?,
                    _ => lower_sql_roots(&one, &catalog)?,
                });
                entries.extend(one.query_workload.entries());
                let mut query = workload_queries(&one).remove(0);
                query["id"] = json!(name);
                described.push(query);
            }
            let data = DataWorkload {
                data_ingestion_interval: declared(DurationMs(interval_ms)),
                ..Default::default()
            };
            let result =
                run_planning_stages_over(roots.clone(), &entries, &data, Some(max_combinations));
            document_of(described, &roots, result, max_displayed)?
        }
        _ => return Err("give exactly one of --example or --promql/--sql queries".into()),
    };
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}").into())
}

/// The document for `run`, whose workload queries are `queries`.
fn stage_pipeline(queries: Vec<Value>, run: &PlanningStagesRun, max_displayed: usize) -> Value {
    // The display keeps the first `max_displayed` candidates and the
    // selected one; plan selection has already ranked all of them.
    let selected_logical = run
        .stage3_plan_selection
        .as_ref()
        .ok()
        .and_then(|selection| {
            run.stage2_physical_asap_optimization
                .iter()
                .find(|p| p.id == selection.selected)
                .map(|p| p.from_logical.as_str())
        });
    let shown: HashSet<&str> = run
        .stage1_logical_asap_optimization
        .iter()
        .enumerate()
        .filter(|(index, c)| *index < max_displayed || Some(c.id.as_str()) == selected_logical)
        .map(|(_, c)| c.id.as_str())
        .collect();
    let shown_physical: HashSet<&str> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| shown.contains(p.from_logical.as_str()))
        .map(|p| p.id.as_str())
        .collect();
    // Labels are for this document only; planning never reads them.
    let labels = candidate_labels(run);
    // A choice that composition cannot substitute yet is listed with its
    // error rather than ending the run.
    let candidates: Vec<_> = run
        .stage1_logical_asap_optimization
        .iter()
        .filter(|c| shown.contains(c.id.as_str()))
        .map(|c| match &c.roots {
            Ok(roots) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "dag": export(roots) })
            }
            Err(error) => {
                json!({ "id": c.id, "display_name": labels[&c.id], "error": error.to_string() })
            }
        })
        .collect();
    let physical: Vec<_> = run
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| shown_physical.contains(p.id.as_str()))
        .map(|p| json!({ "id": p.id, "from_logical": p.from_logical, "display_name": labels[&p.id], "dag": p.dag }))
        .collect();
    let pruned: Vec<_> = run
        .stage2_pruned
        .iter()
        .filter(|p| shown.contains(p.from_logical.as_str()))
        .map(|p| json!({ "from_logical": p.from_logical, "display_name": labels[&p.from_logical], "reason": p.reason }))
        .collect();
    json!({
        "format": "asap-stage-pipeline/v1",
        "workload": { "queries": queries },
        "stage0_query_language_frontends": { "dag": export(&run.stage0_query_language_frontends) },
        "stage1_logical_asap_optimization": {
            "combinations": run.stage1_logical_asap_optimization.len(),
            "capped": candidates.len() < run.stage1_logical_asap_optimization.len(),
            "candidates": candidates,
        },
        "stage2_physical_asap_optimization": { "candidates": physical, "pruned": pruned },
        "stage3_plan_selection": plan_selection_json(&run.stage3_plan_selection, |id| shown_physical.contains(id)),
    })
}

/// Plan selection over every candidate, listing the costs and rejections
/// of the `shown` ones.
fn plan_selection_json(
    selection: &Result<Selection, SelectionError>,
    shown: impl Fn(&str) -> bool,
) -> Value {
    let rejections_json = |rejected: &[Rejection]| -> Vec<Value> {
        rejections_json(rejected.iter().filter(|r| shown(&r.id)))
    };
    let selection = match selection {
        Ok(selection) => selection,
        Err(SelectionError::NoValidCandidate(rejected)) => {
            return json!({ "costs": {}, "selected": null, "rejected": rejections_json(rejected) })
        }
        Err(error) => {
            return json!({ "costs": {}, "selected": null, "rejected": [], "error": error.to_string() })
        }
    };
    let costs: serde_json::Map<_, _> = selection
        .costs
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, cost)| {
            let per_node: serde_json::Map<_, _> = cost
                .per_node
                .iter()
                .map(|(node, c)| {
                    (
                        node.to_string(),
                        json!({ "cost": c.cost, "multiplier": c.multiplier, "detail": c.detail }),
                    )
                })
                .collect();
            (
                id.clone(),
                json!({ "total": cost.total, "unit": cost.unit, "source": cost.source, "per_node": per_node }),
            )
        })
        .collect();
    let accepted: serde_json::Map<_, _> = selection
        .accepted
        .iter()
        .filter(|(id, _)| shown(id))
        .map(|(id, queries)| {
            let queries: Vec<Value> = queries
                .iter()
                .map(|accepted| {
                    accepted.as_ref().map_or(Value::Null, |accepted| {
                        json!({
                            "target_metric": accepted.target_metric,
                            "accepted_metric": accepted.accepted_metric,
                            "sketches": accepted.sketches,
                            "summary": accepted.to_string(),
                        })
                    })
                })
                .collect();
            (id.clone(), Value::Array(queries))
        })
        .collect();
    json!({
        "costs": costs,
        "accepted": accepted,
        "selected": selection.selected,
        "rejected": rejections_json(&selection.rejected),
    })
}

fn rejections_json<'a>(rejected: impl Iterator<Item = &'a Rejection>) -> Vec<Value> {
    rejected
        .map(|r| json!({ "id": r.id, "valid": r.valid, "reason": r.reason }))
        .collect()
}

/// A logical DAG as a flat node list: node `i` is `nodes[i]`, and children are
/// node indices inside each operator.
fn export(roots: &[QueryRoot]) -> FlatDag {
    flatten(roots).0
}

fn workload_queries(workload: &PlanningWorkload) -> Vec<Value> {
    let language = match workload.query_workload.language {
        QueryLanguage::PromQL => "promql",
        _ => "sql",
    };
    workload
        .query_workload
        .entries()
        .enumerate()
        .map(|(index, entry)| {
            let accuracy = json!(entry.requirements.accuracy.clone());
            let mut requirements = json!({ "accuracy": accuracy });
            if let Some(ms) = entry.requirements.max_latency_ms {
                requirements["max_latency_ms"] = json!(ms);
            }
            if let QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) =
                &entry.recurrence
            {
                requirements["repeat_interval_ms"] = json!(interval.0);
            }
            json!({
                "id": format!("q{}", index + 1),
                "language": language,
                "text": entry.query.0,
                "requirements": requirements,
            })
        })
        .collect()
}

/// `queries` with, for each approximate target that sets no metric, the
/// metric its root's result is naturally stated in
/// ([`default_metric_for_root`]; null for a quantile, which has none) as
/// `requirements.default_metric`. Display only: Stage 3 accepts any metric.
fn with_default_metrics(mut queries: Vec<Value>, roots: &[QueryRoot]) -> Vec<Value> {
    for (query, root) in queries.iter_mut().zip(roots) {
        let accuracy = &query["requirements"]["accuracy"];
        let Ok(target) = serde_json::from_value::<AccuracyTarget>(accuracy.clone()) else {
            continue;
        };
        if target.metric.is_none() && !target.is_exact() {
            query["requirements"]["default_metric"] = json!(default_metric_for_root(root));
        }
    }
    queries
}

/// The tables `--table` declares.
fn sql_catalog(tables: &[String]) -> Result<SqlCatalog, String> {
    let mut catalog = SqlCatalog::new();
    let mut names = HashSet::new();
    for raw in tables {
        let table: Value = serde_json::from_str(raw).map_err(|e| format!("--table: {e}"))?;
        let name = table["name"]
            .as_str()
            .ok_or("--table: name must be a string")?;
        if !names.insert(name.to_owned()) {
            return Err(format!("--table: table {name} is declared twice"));
        }
        let columns = table["columns"]
            .as_array()
            .ok_or("--table: columns must be an array")?
            .iter()
            .map(|column| {
                let column_name = column["name"]
                    .as_str()
                    .ok_or("--table: column name must be a string")?;
                let dtype = match column["type"]
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("timestamp") => DataType::Timestamp,
                    Some("utf8" | "string") => DataType::Utf8,
                    Some("float64" | "double") => DataType::Float64,
                    Some("int64" | "bigint") => DataType::Int64,
                    other => return Err(format!("--table: unsupported column type {other:?}")),
                };
                let nullable = column["nullable"].as_bool().unwrap_or(true);
                Ok(Field::plain(column_name, dtype, nullable))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let schema = match table.get("time_index") {
            None | Some(Value::Null) => Schema::new(columns),
            Some(index) => match index.as_u64().map(|index| index as usize) {
                Some(index) if index < columns.len() => {
                    Schema::with_time_index(columns, index, vec![])
                }
                _ => {
                    return Err(format!(
                        "--table {name}: time_index must be the index of one of its {} columns",
                        columns.len()
                    ))
                }
            },
        };
        catalog = catalog.with_table(name, schema);
    }
    Ok(catalog)
}

fn query_batch(
    language: QueryLanguage,
    queries: &[String],
    accuracy: AccuracyTarget,
    interval_ms: u64,
) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
            query_batch: Some(
                queries
                    .iter()
                    .map(|query| BatchEntry {
                        query: Query(query.clone()),
                        requirements: QueryRequirements {
                            accuracy: accuracy.clone(),
                            ..Default::default()
                        },
                        predictability: Default::default(),
                        invocations: 1,
                        execute_at: None,
                        time_selection: Default::default(),
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: declared(DurationMs(interval_ms)),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no valid candidate, the document is still written: `selected`
    /// is null and every rejection is listed.
    #[test]
    fn failed_selection_lists_rejections_without_a_selection() {
        let rejected = vec![Rejection {
            id: "P1".into(),
            valid: false,
            reason: "q1: misses target".into(),
        }];
        let document =
            plan_selection_json(&Err(SelectionError::NoValidCandidate(rejected)), |_| true);
        assert_eq!(
            document,
            json!({
                "costs": {},
                "selected": null,
                "rejected": [{ "id": "P1", "valid": false, "reason": "q1: misses target" }],
            })
        );
    }

    /// With nothing else displayed, the document still shows the selected
    /// candidate and its cost, chosen among all candidates.
    #[test]
    fn display_cap_keeps_the_selected_candidate() {
        let workload = query_batch(
            QueryLanguage::PromQL,
            &["sum(m)".into()],
            AccuracyTarget::exact(),
            15_000,
        );
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let document = stage_pipeline(workload_queries(&workload), &run, 0);
        let selected = document["stage3_plan_selection"]["selected"]
            .as_str()
            .unwrap();
        let physical = document["stage2_physical_asap_optimization"]["candidates"]
            .as_array()
            .unwrap();
        assert_eq!(physical.len(), 1);
        assert_eq!(physical[0]["id"], selected);
        assert!(document["stage3_plan_selection"]["costs"]
            .get(selected)
            .is_some());
        assert_eq!(document["stage3_plan_selection"]["rejected"], json!([]));
    }
}
