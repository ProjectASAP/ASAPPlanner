//! The one list of in-repo workloads that the contracts suite, the planner
//! snapshots and the oracles read (`docs/develop_docs/testing.md`), and the
//! builders that plan a workload only through the production path: the
//! query-language frontends, then Stage 0 to Stage 3 as `asap_planner`
//! runs them.
//!
//! [`corpus_workloads`] reads every source from the repository, so a
//! workload added to one of them is planned by every suite that iterates
//! the registry:
//!
//! | Id prefix | Source |
//! |---|---|
//! | `o11y_bench/` | `crates/devtools/tests/corpus_snapshots/o11y_bench/corpus.json`, one workload per query |
//! | `sketch_plans/` | `crates/devtools/tests/sketch_plan_snapshots/manifest.json` (MVP and raw-retained deployments) |
//! | `planner_layering/` | `tools/dag-viewer/workloads/planner-layering/` |
//! | `memo_examples/` | `tools/dag-viewer/memo-examples/{promql,sql}/` |
//! | `random_mixed/` | the sets of `tools/dag-viewer/workloads/index.json` |
//! | `fuzz_seeds/` | `crates/devtools/tests/planner_fuzz_seeds/` (planned as `planner_fuzz` plans: accuracy not enforced) |
//!
//! No ASAPWorkloads workload files exist in this repository (its query
//! catalog appears only as `planner_fuzz` generator patterns), so there is
//! no source for them.

use std::path::{Path, PathBuf};

use asap_cost::CostInputs;
use asap_planner::PlanSearchSettings;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, PlanningWorkload, QueryLanguage, QueryWorkloadEntry, SourceFacts,
};
use serde_json::{json, Value};

use crate::planner_fuzz::{read_seed_file, FuzzWorkload};
use crate::planning_stages::{
    lower_workload, planning_models, run_search_over, run_stages_until, sql_catalog,
    with_executor_summaries, PlanningStagesRun, RunUntil,
};

/// Every source's id prefix, in [`corpus_workloads`] order: a workload's id
/// is `<source>/<name>`.
pub const SOURCES: [&str; 6] = [
    "o11y_bench",
    "sketch_plans",
    "planner_layering",
    "memo_examples",
    "random_mixed",
    "fuzz_seeds",
];

/// One registry workload with the deployment it is planned for: what
/// `asap_planner` is given for it.
#[derive(Debug, Clone)]
pub struct CorpusWorkload {
    /// `<source>/<name>`, unique in the registry, e.g. `o11y_bench/q21`.
    pub id: String,
    /// The workload as a user writes it (`asap_planner --workload`).
    pub workload: PlanningWorkload,
    /// A SQL workload's tables, each an `asap_planner --sql-catalog-table`
    /// JSON object; empty for PromQL.
    pub sql_tables: Vec<Value>,
    /// What the deployment's executor runs and keeps
    /// (`--deployment-capabilities`); `supported_summaries: None` plans with
    /// the executor's summary families, as `asap_planner` does.
    pub deployment_capabilities: DeploymentCapabilities,
    /// The deployment's cost model and unit costs (`--cost-inputs`).
    pub cost_inputs: CostInputs,
    /// Plan with accuracy not enforced (`--ignore-accuracy-guarantees`).
    pub ignore_accuracy_guarantees: bool,
}

/// A workload planned through the production path, with what costing its
/// plan needs besides the cost inputs it was planned with.
#[derive(Debug)]
pub struct PlannedWorkload {
    /// One entry per query, in workload order (`QueryWorkload::entries`).
    pub entries: Vec<QueryWorkloadEntry>,
    /// The data workload (empty when the workload declares none).
    pub data: DataWorkload,
    /// Every stage's output up to the one planned until.
    pub stages: PlanningStagesRun,
}

/// `workload` planned as `asap_planner --run-until <until>` plans it, with
/// the binary's lowering (`planning_stages::lower_workload`, SQL over
/// `sql_tables`) and models (`planning_stages::planning_models` over
/// `with_executor_summaries`): validated, lowered, then Stage 0 up to
/// `until`. Until Stage 3 it runs the facade's `run_planning` (with
/// `inspect`, so every stage's output is kept), as `planner_fuzz` plans.
/// A stage that fails after Pass 1 is reported in the run, as
/// `asap_planner` reports it; validation, lowering and Pass 1 errors are
/// returned.
pub fn plan_workload(
    workload: &PlanningWorkload,
    sql_tables: &[Value],
    deployment_capabilities: &DeploymentCapabilities,
    cost_inputs: &CostInputs,
    ignore_accuracy_guarantees: bool,
    until: RunUntil,
) -> Result<PlannedWorkload, String> {
    workload.validate().map_err(|e| e.to_string())?;
    let tables: Vec<String> = sql_tables.iter().map(Value::to_string).collect();
    let roots = lower_workload(workload, &sql_catalog(&tables)?.0)?;
    let entries: Vec<QueryWorkloadEntry> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let capabilities = with_executor_summaries(deployment_capabilities.clone());
    let models = planning_models(&capabilities, cost_inputs, ignore_accuracy_guarantees);
    let stages = match until {
        RunUntil::Stage3 => run_search_over(
            roots,
            &entries,
            &data,
            models,
            PlanSearchSettings { inspect: true },
        )?,
        _ => run_stages_until(roots, &entries, &data, models, until)?,
    };
    Ok(PlannedWorkload {
        entries,
        data,
        stages,
    })
}

impl CorpusWorkload {
    /// This workload planned for its deployment up to `until` ([`plan_workload`]).
    pub fn plan(&self, until: RunUntil) -> Result<PlannedWorkload, String> {
        plan_workload(
            &self.workload,
            &self.sql_tables,
            &self.deployment_capabilities,
            &self.cost_inputs,
            self.ignore_accuracy_guarantees,
            until,
        )
    }

    /// The `asap_planner` arguments for this workload, its input files
    /// written to `directory`: `--workload`, each `--sql-catalog-table`,
    /// `--deployment-capabilities`, `--cost-inputs` and
    /// `--ignore-accuracy-guarantees` when set.
    pub fn asap_planner_args(&self, directory: &Path) -> Result<Vec<String>, String> {
        let file_name = self.id.replace('/', "__");
        let write = |suffix: &str, value: String| -> Result<String, String> {
            let path = directory.join(format!("{file_name}.{suffix}.json"));
            std::fs::write(&path, value).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(path.display().to_string())
        };
        fn to_json(value: &impl serde::Serialize) -> Result<String, String> {
            serde_json::to_string(value).map_err(|e| e.to_string())
        }
        let mut args = vec![
            "--workload".to_owned(),
            write("workload", to_json(&self.workload)?)?,
        ];
        for table in &self.sql_tables {
            args.extend(["--sql-catalog-table".to_owned(), table.to_string()]);
        }
        args.extend([
            "--deployment-capabilities".to_owned(),
            write("capabilities", to_json(&self.deployment_capabilities)?)?,
            "--cost-inputs".to_owned(),
            write("cost_inputs", to_json(&self.cost_inputs)?)?,
        ]);
        if self.ignore_accuracy_guarantees {
            args.push("--ignore-accuracy-guarantees".to_owned());
        }
        Ok(args)
    }
}

/// The repository root.
fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| format!("{what}: {e}"))
}

/// `object[key]` as a string, or an error naming `what` and the key.
fn string_field<'a>(object: &'a Value, key: &str, what: &str) -> Result<&'a str, String> {
    object[key]
        .as_str()
        .ok_or_else(|| format!("{what}: no {key}"))
}

/// Whether a manifest's or index's `accuracy_guarantees` (`"enforced"`,
/// the default, or `"ignored"`) ignores them.
fn accuracy_guarantees_ignored(value: &Value, what: &str) -> Result<bool, String> {
    match value.as_str() {
        None | Some("enforced") => Ok(false),
        Some("ignored") => Ok(true),
        Some(other) => Err(format!("{what}: accuracy_guarantees {other}")),
    }
}

/// A workload of the default deployment (raw data retained, the
/// executor's summary families, cost model 1 with illustrative unit costs),
/// accuracy enforced.
fn default_deployment(
    id: String,
    workload: PlanningWorkload,
    sql_tables: Vec<Value>,
) -> CorpusWorkload {
    CorpusWorkload {
        id,
        workload,
        sql_tables,
        deployment_capabilities: DeploymentCapabilities::default(),
        cost_inputs: CostInputs::default(),
        ignore_accuracy_guarantees: false,
    }
}

/// Every registry workload, in source order (see the module docs). Errors
/// name the file that does not parse.
pub fn corpus_workloads() -> Result<Vec<CorpusWorkload>, String> {
    let mut workloads = o11y_bench()?;
    workloads.extend(sketch_plans()?);
    workloads.extend(planner_layering()?);
    workloads.extend(memo_examples()?);
    workloads.extend(random_mixed()?);
    workloads.extend(fuzz_seeds()?);
    Ok(workloads)
}

/// The o11y benchmark corpus: one repeating query per workload under the
/// corpus's shared entry and data, each id `o11y_bench/<query id>`; a query
/// with a `skip_reason` is left out.
pub fn o11y_bench() -> Result<Vec<CorpusWorkload>, String> {
    let path =
        repository_root().join("crates/devtools/tests/corpus_snapshots/o11y_bench/corpus.json");
    let corpus = read_json(&path)?;
    let queries = corpus["queries"]
        .as_array()
        .ok_or("corpus.json: no queries")?;
    queries
        .iter()
        .filter(|query| query.get("skip_reason").is_none())
        .map(|query| {
            let mut entry = corpus["query_workload"]["repeating_query"].clone();
            entry["query"] = query["query"].clone();
            let workload = json!({
                "query_workload": {
                    "language": corpus["query_workload"]["language"],
                    "repeating_queries": [entry],
                },
                "data_workload": corpus["data_workload"],
            });
            let id = string_field(query, "id", "corpus.json: a query")?;
            Ok(default_deployment(
                format!("o11y_bench/{id}"),
                parse(workload, id)?,
                Vec::new(),
            ))
        })
        .collect()
}

/// The sketch plan snapshots' workloads (`manifest.json`), each with its
/// deployment's capabilities and cost inputs, ids `sketch_plans/<id>`.
pub fn sketch_plans() -> Result<Vec<CorpusWorkload>, String> {
    let root = repository_root();
    let manifest =
        read_json(&root.join("crates/devtools/tests/sketch_plan_snapshots/manifest.json"))?;
    let entries = manifest["workloads"]
        .as_array()
        .ok_or("manifest.json: no workloads")?;
    entries
        .iter()
        .map(|entry| {
            let id = string_field(entry, "id", "manifest.json: an entry")?;
            let deployment_name = string_field(entry, "deployment", id)?;
            let deployment: Vec<String> = parse(
                manifest["deployments"]
                    .get(deployment_name)
                    .ok_or_else(|| format!("{id}: no deployment {deployment_name}"))?
                    .clone(),
                &format!("{id}: deployment"),
            )?;
            let mut workload = default_deployment(
                format!("sketch_plans/{id}"),
                parse(
                    read_json(&root.join(string_field(entry, "workload", id)?))?,
                    id,
                )?,
                match entry.get("tables") {
                    Some(_) => parse(
                        read_json(&root.join(string_field(entry, "tables", id)?))?,
                        id,
                    )?,
                    None => Vec::new(),
                },
            );
            for pair in deployment.chunks(2) {
                match pair {
                    [flag, file] if flag == "--deployment-capabilities" => {
                        workload.deployment_capabilities =
                            parse(read_json(&root.join(file))?, file)?
                    }
                    [flag, file] if flag == "--cost-inputs" => {
                        workload.cost_inputs = parse(read_json(&root.join(file))?, file)?
                    }
                    other => return Err(format!("{id}: unknown deployment argument {other:?}")),
                }
            }
            workload.ignore_accuracy_guarantees =
                accuracy_guarantees_ignored(&entry["accuracy_guarantees"], id)?;
            Ok(workload)
        })
        .collect()
}

/// The `*.json` workloads of `directory` (except `tables.json`), sorted by
/// name, with the directory's `tables.json` when it has one.
fn workload_files(directory: &Path) -> Result<Vec<(String, PlanningWorkload, Vec<Value>)>, String> {
    let tables: Vec<Value> = match directory.join("tables.json").is_file() {
        true => parse(read_json(&directory.join("tables.json"))?, "tables.json")?,
        false => Vec::new(),
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory)
        .map_err(|e| format!("{}: {e}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    files.retain(|path| {
        path.extension().is_some_and(|ext| ext == "json") && !path.ends_with("tables.json")
    });
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let workload: PlanningWorkload = parse(read_json(&path)?, &path.display().to_string())?;
            let tables = match workload.query_workload.language {
                QueryLanguage::PromQL => Vec::new(),
                _ => tables.clone(),
            };
            Ok((name, workload, tables))
        })
        .collect()
}

/// The #509 planner-layering examples as the viewer plans them (default
/// deployment, accuracy enforced), ids `planner_layering/<example>`.
fn planner_layering() -> Result<Vec<CorpusWorkload>, String> {
    let directory = repository_root().join("tools/dag-viewer/workloads/planner-layering");
    Ok(workload_files(&directory)?
        .into_iter()
        .map(|(name, workload, tables)| {
            default_deployment(format!("planner_layering/{name}"), workload, tables)
        })
        .collect())
}

/// The viewer's memo examples (default deployment, accuracy enforced), ids
/// `memo_examples/<language>/<name>`.
fn memo_examples() -> Result<Vec<CorpusWorkload>, String> {
    let mut workloads = Vec::new();
    for language in ["promql", "sql"] {
        let directory = repository_root()
            .join("tools/dag-viewer/memo-examples")
            .join(language);
        for (name, workload, tables) in workload_files(&directory)? {
            workloads.push(default_deployment(
                format!("memo_examples/{language}/{name}"),
                workload,
                tables,
            ));
        }
    }
    Ok(workloads)
}

/// The viewer's random mixed workload sets (`index.json`), each workload
/// with its set's deployment capabilities, cost model and accuracy, ids
/// `random_mixed/<set>/<workload>`.
fn random_mixed() -> Result<Vec<CorpusWorkload>, String> {
    let directory = repository_root().join("tools/dag-viewer/workloads");
    let index = read_json(&directory.join("index.json"))?;
    let mut workloads = Vec::new();
    for set in index["sets"].as_array().ok_or("index.json: no sets")? {
        let set_id = string_field(set, "id", "index.json: a set")?;
        let capabilities: DeploymentCapabilities =
            parse(set["deployment_capabilities"].clone(), set_id)?;
        let cost_inputs: CostInputs = match set["cost_model"].as_str() {
            Some(name) => parse(
                read_json(
                    &repository_root().join(format!("tools/dag-viewer/cost-inputs/{name}.json")),
                )?,
                name,
            )?,
            None => CostInputs::default(),
        };
        let ignore = accuracy_guarantees_ignored(&set["accuracy_guarantees"], set_id)?;
        for workload in set["workloads"]
            .as_array()
            .ok_or("index.json: no workloads")?
        {
            let id = string_field(workload, "id", set_id)?;
            let file = directory.join(string_field(workload, "file", id)?);
            workloads.push(CorpusWorkload {
                id: format!("random_mixed/{set_id}/{id}"),
                workload: parse(read_json(&file)?, &file.display().to_string())?,
                sql_tables: Vec::new(),
                deployment_capabilities: capabilities.clone(),
                cost_inputs: cost_inputs.clone(),
                ignore_accuracy_guarantees: ignore,
            });
        }
    }
    Ok(workloads)
}

/// The `planner_fuzz` seed workloads as the tester reads them, with their
/// registry ids `fuzz_seeds/<file stem>` (e.g. `fuzz_seeds/issue902`).
pub fn fuzz_seed_workloads() -> Result<Vec<(String, FuzzWorkload)>, String> {
    let directory = repository_root().join("crates/devtools/tests/planner_fuzz_seeds");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .map_err(|e| format!("{}: {e}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    files.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            let name = name.trim_end_matches(".json").trim_end_matches(".workload");
            Ok((format!("fuzz_seeds/{name}"), read_seed_file(&path)?))
        })
        .collect()
}

/// The `planner_fuzz` seeds, planned as the tester plans them: accuracy not
/// enforced, default cost inputs, the seed's deployment.
fn fuzz_seeds() -> Result<Vec<CorpusWorkload>, String> {
    Ok(fuzz_seed_workloads()?
        .into_iter()
        .map(|(id, seed)| CorpusWorkload {
            id,
            workload: seed.planning_workload(),
            sql_tables: seed.tables.clone(),
            deployment_capabilities: seed.deployment(),
            cost_inputs: CostInputs::default(),
            ignore_accuracy_guarantees: true,
        })
        .collect())
}

/// The ingesting arrival at the sum of `data`'s sources' own rates
/// (`series_count` per `sample_interval`), when every source declares one
/// and the sum is positive.
pub fn arrival_from_sources(data: &DataWorkload) -> Option<DataArrival> {
    let rows_per_second: f64 = data
        .sources
        .values()
        .map(|facts| match (facts.series_count, facts.sample_interval) {
            (Some(series), Some(interval)) if interval.0 > 0 => {
                Some(series as f64 * 1_000.0 / interval.0 as f64)
            }
            _ => None,
        })
        .sum::<Option<f64>>()?;
    (rows_per_second > 0.0).then_some(DataArrival::Ingesting {
        rows_per_second,
        max_out_of_order_ms: None,
    })
}

/// A PromQL workload of `queries`, each repeating every `interval_ms` at
/// phase 0 with accuracy `accuracy`, over `sources` (metric → its facts),
/// arriving at the sum of the sources' rates when each declares
/// `series_count` and `sample_interval` ([`arrival_from_sources`]). For
/// rule and oracle tests that plan a small workload through the production
/// path ([`plan_workload`]).
pub fn repeating_promql_workload(
    queries: &[&str],
    interval_ms: u64,
    accuracy: AccuracyTarget,
    sources: &[(&str, SourceFacts)],
) -> PlanningWorkload {
    let entries: Vec<Value> = queries
        .iter()
        .map(|query| {
            json!({
                "query": query,
                "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": 0}},
                "requirements": {"accuracy": accuracy},
                "predictability": {"predictable": {"known_at": null}},
            })
        })
        .collect();
    let mut workload: PlanningWorkload = parse(
        json!({
            "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": entries},
            "data_workload": {"arrival": "unknown"},
        }),
        "repeating_promql_workload",
    )
    .expect("a repeating PromQL workload deserializes");
    let data = workload.data_workload.as_mut().expect("declared above");
    data.sources = sources
        .iter()
        .map(|(name, facts)| (name.to_string(), facts.clone()))
        .collect();
    if let Some(arrival) = arrival_from_sources(data) {
        data.arrival = arrival;
    }
    workload
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_planner::SelectionError;
    use asap_types::ir::QueryRoot;
    use asap_types::workload::DurationMs;

    fn facts(series_count: u64, sample_interval_ms: u64) -> SourceFacts {
        SourceFacts {
            series_count: Some(series_count),
            sample_interval: Some(DurationMs(sample_interval_ms)),
            ..SourceFacts::default()
        }
    }

    /// Every source loads with the workload count of its files today, ids
    /// are unique, and named workloads of each source are present. The
    /// o11y count is its committed snapshots (`<id>.json`), a listing the
    /// registry does not read.
    #[test]
    fn every_source_loads_with_unique_ids() {
        let workloads = corpus_workloads().unwrap();
        let mut ids: Vec<&str> = workloads.iter().map(|w| w.id.as_str()).collect();
        ids.sort();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate ids");
        let o11y_snapshots = std::fs::read_dir(
            repository_root().join("crates/devtools/tests/corpus_snapshots/o11y_bench"),
        )
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            let name = name.to_string_lossy();
            name.ends_with(".json") && name != "corpus.json"
        })
        .count();
        let expected = [
            ("o11y_bench", o11y_snapshots),
            ("sketch_plans", 10),
            ("planner_layering", 4),
            ("memo_examples", 7),
            ("random_mixed", 30),
            ("fuzz_seeds", 3),
        ];
        assert_eq!(expected.map(|(source, _)| source), SOURCES);
        for (source, count) in expected {
            let prefix = format!("{source}/");
            assert_eq!(
                ids.iter().filter(|id| id.starts_with(&prefix)).count(),
                count,
                "{source}"
            );
        }
        for id in [
            "o11y_bench/q21",
            "sketch_plans/shared-quantiles-1h-mvp",
            "planner_layering/example2",
            "memo_examples/sql/rollupheavy-t11-service-endpoint-5m",
            "fuzz_seeds/issue902",
        ] {
            assert!(ids.contains(&id), "no {id}");
        }
    }

    /// Two sources at 1,000 series every 1 s and 100 series every 10 s
    /// arrive at 1,010 rows per second; each query repeats every 60 s.
    #[test]
    fn a_repeating_promql_workload_arrives_at_its_sources_rates() {
        let workload = repeating_promql_workload(
            &["sum(a)", "sum(b)"],
            60_000,
            AccuracyTarget::exact(),
            &[("a", facts(1000, 1000)), ("b", facts(100, 10_000))],
        );
        assert_eq!(
            workload.data_workload.unwrap().arrival.rows_per_second(),
            Some(1_010.0)
        );
        assert_eq!(workload.query_workload.entries().count(), 2);
    }

    /// Planning until Pass 1 runs the frontends and Stage 0, keeps the
    /// series identity the PromQL lowering adds on every `Scan`, and leaves
    /// the later stages empty.
    #[test]
    fn planning_until_a_stage_leaves_the_later_ones_empty() {
        let workload = repeating_promql_workload(
            &["sum(rate(m[5m]))"],
            60_000,
            AccuracyTarget::exact(),
            &[("m", facts(10, 1000))],
        );
        let planned = plan_workload(
            &workload,
            &[],
            &DeploymentCapabilities::default(),
            &CostInputs::default(),
            false,
            RunUntil::Pass1,
        )
        .unwrap();
        let run = &planned.stages.run;
        assert!(run.pass1_memo.is_some());
        assert!(run.logical_asap_candidates.is_none());
        assert!(run.physical_asap_candidates.is_none());
        assert!(matches!(run.selection, Err(SelectionError::NotRun)));
        let QueryRoot::Operator(root) = &planned.stages.stage0_query_language_frontends[0] else {
            panic!("an operator root")
        };
        let scan_schemas: Vec<_> = asap_types::ir::OperatorNode::reachable(root)
            .into_iter()
            .filter(|node| {
                matches!(
                    node.non_asap(),
                    Some(asap_types::ir::NonASAPOp::Scan { .. })
                )
            })
            .map(|node| node.schema.clone())
            .collect();
        assert!(!scan_schemas.is_empty(), "no Scan reached from the root");
        assert!(
            scan_schemas.iter().all(|schema| schema
                .fields
                .iter()
                .any(|field| field.name == asap_types::ir::schema::PROMQL_SERIES_IDENTITY)),
            "{scan_schemas:?}"
        );
    }

    /// A MetricsQL workload is an unsupported language here, not a SQL
    /// parse error.
    #[test]
    fn a_language_without_a_frontend_here_is_reported_as_unsupported() {
        let mut workload = repeating_promql_workload(
            &["sum(m)"],
            60_000,
            AccuracyTarget::exact(),
            &[("m", facts(10, 1000))],
        );
        workload.query_workload.language = QueryLanguage::MetricsQL;
        let error = plan_workload(
            &workload,
            &[],
            &DeploymentCapabilities::default(),
            &CostInputs::default(),
            false,
            RunUntil::Stage0,
        )
        .unwrap_err();
        assert!(error.contains("unsupported query language"), "{error}");
    }
}
