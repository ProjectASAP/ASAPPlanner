//! The memo's value bounds against execution: every bound the memo gives
//! on the values one evaluation of a group returns
//! (`Memo::max_values_per_evaluation`: the readout values R a whole-result
//! failure probability splits over, and the bound on any grouped result)
//! and on a query's values (n_q) must hold on every dataset the declared
//! source facts allow. Each query of a corpus is lowered as planning lowers
//! it, its Stage 0 memo built, and the query run by the executor over
//! small datasets that keep to the facts (`series_count`, the distinct
//! values per label and per label set, `sample_interval`, `rows_held`);
//! each group's result rows must be at most its bound. The datasets seek
//! the worst case: the labels and columns the facts bound only together
//! take independent, crossed or aligned values across series and rows,
//! series can be replaced between the times a query reads (`offset`), and
//! labels compared with literals split the series into blocks that a match
//! pairs one to one. So a bound that takes two scans for one (a join, or a
//! many-to-one match of one metric), misses a NULL an outer join adds, or
//! reads a declared count where none applies fails here, whichever
//! operator it comes from.
//!
//! Example: `quantile by (job, instance) (0.5, m{side="l"} * on(x)
//! group_left(instance) m{side="r"})` over `m` declaring 200 series and 10
//! (`job`, `instance`) pairs: the left series of a block take random pairs
//! and the right series of the same `x` other random pairs, so the result
//! holds up to 100 (`job`, `instance`) values; a bound of 10 fails.
//!
//! A readout's R is its group's bound; the readout reads the values the
//! group's exact aggregate returns, which the executor counts here.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::Arc;

use asap_devtools::planning_stages::{lower_promql_roots, lower_sql_roots, sql_catalog};
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, SchemaRef, Value};
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, FieldDataType};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload;
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::workload::{DataWorkload, PlanningWorkload, SourceFacts};
use futures::executor::block_on;
use futures::TryStreamExt;
use serde_json::{json, Value as Json};

/// When every query is evaluated: late enough for any lookback.
const EVALUATION_MS: i64 = 30 * 86_400_000;

/// The default corpus: the query shapes each bound rule was reviewed
/// against, over the facts they were reviewed with.
const CASES: &str = include_str!("memo_value_bounds_fixtures/cases.json");

/// How large and how many datasets each query is run over.
#[derive(Clone, Copy)]
struct DatasetSizes {
    /// The most series (PromQL) or groups of rows (SQL) a source gets; a
    /// source declaring more gets this many, which its facts allow.
    max_units: u64,
    /// The most samples per series, or rows per group of rows.
    max_samples_per_unit: u64,
    /// Datasets per query beyond the fixed worst-case seekers.
    random_datasets: u64,
}

const DEFAULT_SIZES: DatasetSizes = DatasetSizes {
    max_units: 240,
    max_samples_per_unit: 20,
    random_datasets: 6,
};

const LARGE_SIZES: DatasetSizes = DatasetSizes {
    max_units: 1200,
    max_samples_per_unit: 40,
    random_datasets: 40,
};

/// The default corpus's bounds the executor cannot run yet (`case:what`,
/// as [`Report::bounds`] names them), each with the operator it lacks. A
/// bound leaves this list once the executor runs it, so the corpus checks
/// every bound the executor can run.
const EXECUTOR_GAPS: &[(&str, &str)] = &[
    ("c_cast_num:g3 Aggregate", "CAST to BIGINT"),
    ("c_cast_num:g4 Project", "CAST to BIGINT"),
    ("c_cast_num:n_q", "CAST to BIGINT"),
    ("c_cast_ts:g3 Aggregate", "CAST to BIGINT"),
    ("c_cast_ts:g4 Project", "CAST to BIGINT"),
    ("c_cast_ts:n_q", "CAST to BIGINT"),
    ("c_sub_alias:g3 Aggregate", "date_trunc"),
    ("c_sub_alias:g4 Project", "date_trunc"),
    ("c_sub_alias:n_q", "date_trunc"),
    ("c_sub_upper:g3 Aggregate", "upper"),
    ("c_sub_upper:g4 Project", "upper"),
    ("c_sub_upper:n_q", "upper"),
    ("d_distinct:g4 Dedup", "DISTINCT"),
    ("d_distinct:n_q", "DISTINCT"),
    (
        "p11_scalar_pv:g5 Project",
        "a binary operator over scalar()",
    ),
    ("p11_scalar_pv:n_q", "a binary operator over scalar()"),
    ("p12_vector_or:n_q", "`or` over vector()"),
    ("p_cv_by:g3 Aggregate", "a quantile over count_values"),
    ("p_cv_by:n_q", "a quantile over count_values"),
    ("p_cv_key:g3 Aggregate", "a quantile over count_values"),
    ("p_cv_key:n_q", "a quantile over count_values"),
    ("p_hist:g2 Aggregate", "rate"),
    ("p_hist:g3 Aggregate", "rate"),
    ("p_hist:g4 Aggregate", "rate"),
    ("p_hist:n_q", "rate"),
    ("p_hist_qot:g2 Aggregate", "rate"),
    ("p_hist_qot:g3 Aggregate", "rate"),
    ("p_hist_qot:n_q", "rate"),
    ("p_hist_whole:g2 Aggregate", "rate"),
    ("p_hist_whole:g3 Aggregate", "rate"),
    ("p_hist_whole:g4 Aggregate", "rate"),
    ("p_hist_whole:n_q", "rate"),
    ("p_labelrep_other:g3 Aggregate", "label_replace"),
    ("p_labelrep_other:n_q", "label_replace"),
    ("p_labelrep_qot_whole:g4 Aggregate", "label_replace"),
    ("p_labelrep_qot_whole:n_q", "label_replace"),
    ("p_labelrep_same:g3 Aggregate", "label_replace"),
    ("p_labelrep_same:n_q", "label_replace"),
    ("p_labelrep_whole:g3 Aggregate", "label_replace"),
    ("p_labelrep_whole:n_q", "label_replace"),
    ("p_subq:g3 Aggregate", "a subquery"),
    ("p_subq:g4 Aggregate", "a subquery"),
    ("p_subq:n_q", "a subquery"),
    ("p_subq_outer:g4 Aggregate", "a subquery"),
    ("p_subq_outer:n_q", "a subquery"),
    ("s1_bucket_nosc:g3 Aggregate", "date_trunc"),
    ("s1_bucket_nosc:g4 Project", "date_trunc"),
    ("s1_bucket_nosc:n_q", "date_trunc"),
    ("s1_bucket_sc:g3 Aggregate", "date_trunc"),
    ("s1_bucket_sc:g4 Project", "date_trunc"),
    ("s1_bucket_sc:n_q", "date_trunc"),
    ("s2_src_bucket_sc:g3 Aggregate", "date_trunc"),
    ("s2_src_bucket_sc:g4 Project", "date_trunc"),
    ("s2_src_bucket_sc:n_q", "date_trunc"),
    ("u_union_mixed:g6 Aggregate", "UNION ALL below a grouping"),
    ("u_union_mixed:g7 Project", "UNION ALL below a grouping"),
    ("u_union_mixed:n_q", "UNION ALL below a grouping"),
    ("u_union_same:g8 Aggregate", "UNION ALL below a grouping"),
    ("u_union_same:g9 Project", "UNION ALL below a grouping"),
    ("u_union_same:n_q", "UNION ALL below a grouping"),
    ("w_window_key:g4 Aggregate", "row_number() as a key"),
    ("w_window_key:g5 Project", "row_number() as a key"),
    ("w_window_key:n_q", "row_number() as a key"),
];

/// The default corpus's bounds the executor runs but that return no values
/// on every dataset, so nothing tests them (`case:what`), each with why no
/// dataset the facts allow fills them. Empty: every bound that runs is
/// reached.
const NEVER_REACHED: &[(&str, &str)] = &[];

/// The default corpus's cases the frontend does not lower.
const FRONTEND_GAPS: &[&str] = &[
    // `src || dst`: string concatenation.
    "g_expr",
    // `offset` on a function of a range selector.
    "p9_offset_or",
];

/// Every memo bound of the default corpus holds on every dataset; every
/// bound runs on at least one dataset and returns values on one, but for
/// the executor's and the frontend's known gaps.
#[test]
fn memo_value_bounds_hold_on_data_the_declared_facts_allow() {
    let report = check_corpus(&default_corpus(), DEFAULT_SIZES);
    report.assert_sound();
    let never_executed = report.never_executed();
    let never_run: Vec<&str> = never_executed.iter().map(|(bound, _)| *bound).collect();
    let gaps: Vec<&str> = EXECUTOR_GAPS.iter().map(|(bound, _)| *bound).collect();
    assert_eq!(never_run, gaps, "{never_executed:#?}");
    let unreached: Vec<&str> = NEVER_REACHED.iter().map(|(bound, _)| *bound).collect();
    assert_eq!(report.never_reached(), unreached);
    assert_eq!(report.not_lowered, FRONTEND_GAPS);
}

/// [`memo_value_bounds_hold_on_data_the_declared_facts_allow`] over larger
/// and more datasets, and also over the checked-in dag-viewer workloads
/// and the workloads `ASAP_MEMO_VALUE_BOUNDS_WORKLOADS` lists (lines of
/// `name|workload.json|tables.json`, the tables optional).
#[test]
#[ignore = "minutes: larger datasets over every corpus"]
fn memo_value_bounds_hold_on_data_the_declared_facts_allow_over_every_corpus() {
    let mut corpus = default_corpus();
    corpus.extend(dag_viewer_corpus());
    if let Ok(manifest) = std::env::var("ASAP_MEMO_VALUE_BOUNDS_WORKLOADS") {
        corpus.extend(manifest_corpus(&manifest));
    }
    let report = check_corpus(&corpus, LARGE_SIZES);
    report.assert_sound();
}

/// One workload of the corpus and the SQL tables it reads.
struct Case {
    name: String,
    workload: PlanningWorkload,
    tables: Vec<String>,
}

fn default_corpus() -> Vec<Case> {
    let corpus: Json = serde_json::from_str(CASES).unwrap();
    let tables: Vec<String> = corpus["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(Json::to_string)
        .collect();
    corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let language = match case["language"].as_str().unwrap() {
                "promql" => json!("promql"),
                _ => json!({"sql": "datafusion_sql"}),
            };
            let workload = json!({
                "query_workload": {
                    "language": language,
                    "repeating_queries": [{
                        "query": case["query"],
                        "demand": {"fixed_interval_at": {"interval": 60_000, "evaluation_phase": 0}},
                        "requirements": {"max_latency_ms": null, "accuracy": {
                            "max_error": 0.05, "failure_probability": 0.01,
                            "accuracy_metric": "relative", "accuracy_scope": "whole_result",
                        }},
                        "predictability": {"predictable": {"known_at": null}},
                    }],
                },
                "data_workload": {
                    "arrival": {"ingesting": {"rows_per_second": 1000.0}},
                    "sources": corpus["facts"][case["facts"].as_str().unwrap()],
                },
            });
            Case {
                name: case["name"].as_str().unwrap().to_string(),
                workload: serde_json::from_value(workload).unwrap(),
                tables: tables.clone(),
            }
        })
        .collect()
}

/// The dag-viewer's checked-in workloads, each with the tables beside it.
fn dag_viewer_corpus() -> Vec<Case> {
    let root = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/dag-viewer/workloads"
    );
    let mut cases = Vec::new();
    for set in std::fs::read_dir(root).unwrap().flatten() {
        if !set.path().is_dir() {
            continue;
        }
        let tables = std::fs::read_to_string(set.path().join("tables.json"))
            .ok()
            .map(|text| table_strings(&text))
            .unwrap_or_default();
        let mut files: Vec<_> = std::fs::read_dir(set.path())
            .unwrap()
            .flatten()
            .map(|file| file.path())
            .filter(|path| path.file_name().is_some_and(|name| name != "tables.json"))
            .collect();
        files.sort();
        for file in files {
            let Ok(workload) = serde_json::from_str(&std::fs::read_to_string(&file).unwrap())
            else {
                continue;
            };
            cases.push(Case {
                name: file.display().to_string(),
                workload,
                tables: tables.clone(),
            });
        }
    }
    cases
}

/// The workloads a manifest of `name|workload.json|tables.json` lines lists.
fn manifest_corpus(manifest: &str) -> Vec<Case> {
    std::fs::read_to_string(manifest)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('|').collect();
            let workload = serde_json::from_str(&std::fs::read_to_string(fields[1]).ok()?).ok()?;
            let tables = fields
                .get(2)
                .filter(|path| !path.is_empty())
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|text| table_strings(&text))
                .unwrap_or_default();
            Some(Case {
                name: fields[0].to_string(),
                workload,
                tables,
            })
        })
        .collect()
}

/// Each table of a JSON array of tables, as `sql_catalog` reads it.
fn table_strings(text: &str) -> Vec<String> {
    let tables: Json = serde_json::from_str(text).unwrap();
    tables
        .as_array()
        .unwrap()
        .iter()
        .map(Json::to_string)
        .collect()
}

/// A bound one dataset broke: the case, the dataset, the group (or the
/// query result), its bound and the values the executor returned.
struct Violation {
    case: String,
    query: usize,
    dataset: String,
    what: String,
    bound: u64,
    observed: u64,
}

/// What the datasets showed of one bound.
#[derive(Default)]
struct BoundCoverage {
    /// The most values any dataset returned; `None` when the executor ran
    /// the bound on no dataset.
    largest_observed: Option<u64>,
    /// Why the executor last rejected the bound or a dataset for it.
    last_rejection: String,
}

#[derive(Default)]
struct Report {
    cases: usize,
    executions: usize,
    rejected_datasets: usize,
    /// Each bound, as `case:what` (`p13_unless:n_q`, `d_distinct:g4
    /// Dedup`), with what the datasets showed of it.
    bounds: BTreeMap<String, BoundCoverage>,
    not_lowered: Vec<String>,
    violations: Vec<Violation>,
}

impl Report {
    /// The bounds no dataset ran, with why, in `case:what` order.
    fn never_executed(&self) -> Vec<(&str, &str)> {
        self.bounds
            .iter()
            .filter(|(_, coverage)| coverage.largest_observed.is_none())
            .map(|(bound, coverage)| (bound.as_str(), coverage.last_rejection.as_str()))
            .collect()
    }

    /// The bounds every dataset ran returned no values for, so no dataset
    /// tested them.
    fn never_reached(&self) -> Vec<&str> {
        self.bounds
            .iter()
            .filter(|(_, coverage)| coverage.largest_observed == Some(0))
            .map(|(bound, _)| bound.as_str())
            .collect()
    }

    fn assert_sound(&self) {
        println!(
            "memo value bounds: {} cases, {} bounds, {} executions, {} datasets the executor \
             rejected, {} bounds never run, {} never reached, not lowered: {:?}, {} violations",
            self.cases,
            self.bounds.len(),
            self.executions,
            self.rejected_datasets,
            self.never_executed().len(),
            self.never_reached().len(),
            self.not_lowered,
            self.violations.len()
        );
        let mut first_per_bound: BTreeMap<(String, usize, String), &Violation> = BTreeMap::new();
        for violation in &self.violations {
            first_per_bound
                .entry((
                    violation.case.clone(),
                    violation.query,
                    violation.what.clone(),
                ))
                .or_insert(violation);
        }
        for violation in first_per_bound.values() {
            println!(
                "VIOLATION {} query {}: {} bounded by {}, {} on dataset {}",
                violation.case,
                violation.query,
                violation.what,
                violation.bound,
                violation.observed,
                violation.dataset
            );
        }
        assert!(
            first_per_bound.is_empty(),
            "{} memo bounds fall below the values the executor returned",
            first_per_bound.len()
        );
    }
}

/// Every case of `corpus` checked, the cases spread over threads.
fn check_corpus(corpus: &[Case], sizes: DatasetSizes) -> Report {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let reports: Vec<Report> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                scope.spawn(move || {
                    let mut report = Report::default();
                    for case in corpus.iter().skip(thread).step_by(threads) {
                        report.cases += 1;
                        check_case(case, sizes, &mut report);
                    }
                    report
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });
    let mut report = Report::default();
    for part in reports {
        report.cases += part.cases;
        report.executions += part.executions;
        report.rejected_datasets += part.rejected_datasets;
        report.bounds.extend(part.bounds);
        report.not_lowered.extend(part.not_lowered);
        report.violations.extend(part.violations);
    }
    report.not_lowered.sort();
    report
}

/// One query's bounds: each group of its memo with a bound, as the node it
/// lowered to, and its own n_q.
struct QueryBounds {
    root: Rc<OperatorNode>,
    /// (what is bounded, the node computing it, its bound)
    bounds: Vec<(String, Rc<OperatorNode>, u64)>,
}

fn check_case(case: &Case, sizes: DatasetSizes, report: &mut Report) {
    let data = case.workload.data_workload.clone().unwrap_or_default();
    let roots = match lower(case) {
        Ok(roots) => roots,
        Err(_) => {
            report.not_lowered.push(case.name.clone());
            return;
        }
    };
    let Ok(query_memo) = asap_stage0_memo::memo_of_queries(&roots, &vec![None; roots.len()], &data)
    else {
        report.not_lowered.push(case.name.clone());
        return;
    };
    let mut memo = Memo::new(&data);
    let mut group_of_node = HashMap::new();
    for (index, root) in roots.iter().enumerate() {
        let QueryRoot::Operator(root) = root else {
            continue;
        };
        let mut bounds = Vec::new();
        let mut seen = BTreeSet::new();
        collect_bounds(root, &mut memo, &mut group_of_node, &mut seen, &mut bounds);
        if let Some(values) = query_memo.queries()[index].number_of_values_per_query_evaluation {
            bounds.push(("n_q".to_string(), root.clone(), values));
        }
        if bounds.is_empty() {
            continue;
        }
        let query = QueryBounds {
            root: root.clone(),
            bounds,
        };
        // Each bound's coverage entry: `case:what`, the query's index
        // added when the case has several.
        let keys: Vec<String> = query
            .bounds
            .iter()
            .map(|(what, _, _)| match roots.len() {
                1 => format!("{}:{what}", case.name),
                _ => format!("{} query {index}:{what}", case.name),
            })
            .collect();
        for key in &keys {
            report.bounds.entry(key.clone()).or_default();
        }
        // The bounds whose nodes the executor compiles: all together, else
        // each alone.
        let all: Vec<usize> = (0..query.bounds.len()).collect();
        let runnable: Vec<usize> = match compiles(&query, &all) {
            Ok(()) => all,
            Err(_) => all
                .into_iter()
                .filter(|&bound| match compiles(&query, &[bound]) {
                    Ok(()) => true,
                    Err(reason) => {
                        report.bounds.get_mut(&keys[bound]).unwrap().last_rejection = reason;
                        false
                    }
                })
                .collect(),
        };
        if runnable.is_empty() {
            continue;
        }
        for dataset in datasets(case, &query.root, &data, sizes) {
            match execute_counts(&query, &runnable, &dataset) {
                Ok(counts) => {
                    report.executions += 1;
                    for (&bound_index, observed) in runnable.iter().zip(counts) {
                        let coverage = report.bounds.get_mut(&keys[bound_index]).unwrap();
                        coverage.largest_observed = coverage.largest_observed.max(Some(observed));
                        let (what, _, bound) = &query.bounds[bound_index];
                        if observed > *bound {
                            report.violations.push(Violation {
                                case: case.name.clone(),
                                query: index,
                                dataset: dataset.name.clone(),
                                what: what.clone(),
                                bound: *bound,
                                observed,
                            });
                        }
                    }
                }
                Err(reason) => {
                    report.rejected_datasets += 1;
                    for &bound_index in &runnable {
                        report
                            .bounds
                            .get_mut(&keys[bound_index])
                            .unwrap()
                            .last_rejection = reason.clone();
                    }
                }
            }
        }
    }
}

fn lower(case: &Case) -> Result<Vec<QueryRoot>, String> {
    match &case.workload.query_workload.language {
        asap_types::workload::QueryLanguage::SQL(_) => {
            let (catalog, _) = sql_catalog(&case.tables)?;
            lower_sql_roots(&case.workload, &catalog)
        }
        _ => lower_promql_roots(&case.workload),
    }
}

/// Each node below `node` (itself included) whose group has a bound, with
/// its group, as Stage 0 adds them ([`Memo::find_or_add_group`]); the
/// group of `node`.
fn collect_bounds(
    node: &Rc<OperatorNode>,
    memo: &mut Memo,
    group_of_node: &mut HashMap<*const OperatorNode, MemoGroupId>,
    seen: &mut BTreeSet<*const OperatorNode>,
    bounds: &mut Vec<(String, Rc<OperatorNode>, u64)>,
) -> MemoGroupId {
    for child in node.children() {
        collect_bounds(child, memo, group_of_node, seen, bounds);
    }
    let group = match group_of_node.get(&Rc::as_ptr(node)) {
        Some(&group) => group,
        None => {
            let expression = node
                .operator
                .map_children(|child| group_of_node[&Rc::as_ptr(child)]);
            let group = memo
                .find_or_add_group(expression)
                .expect("Stage 0 adds every node of a lowered query");
            group_of_node.insert(Rc::as_ptr(node), group);
            group
        }
    };
    if seen.insert(Rc::as_ptr(node)) {
        if let Some(bound) = memo.max_values_per_evaluation(group) {
            let what = format!("g{} {}", group.0, node.operator.kind_name());
            bounds.push((what, node.clone(), bound));
        }
    }
    group
}

/// The result rows of each bounded node of `query` over `dataset`, in
/// `query.bounds` order; why not when the executor rejects the query or
/// the dataset (a many-to-many match, an unsupported operator).
fn execute_counts(
    query: &QueryBounds,
    bounds: &[usize],
    dataset: &Dataset,
) -> Result<Vec<u64>, String> {
    let run = || -> Result<Vec<u64>, String> {
        let dag = query_time_dag(query, bounds)?;
        // One schema per source: every column any of its scans reads.
        let mut schemas: Vec<(Source, asap_types::ir::schema::Schema)> = Vec::new();
        for node in &dag.nodes {
            let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) =
                &node.payload
            else {
                continue;
            };
            match schemas.iter_mut().find(|(bound, _)| bound == source) {
                Some((_, schema)) => {
                    for field in &node.output_schema.fields {
                        if schema.column_id(&field.name).is_none() {
                            schema.fields.push(field.clone());
                        }
                    }
                }
                None => schemas.push((source.clone(), node.output_schema.clone())),
            }
        }
        let mut store = InMemoryDataStore::default();
        for (source, schema) in schemas {
            let source = &source;
            let schema: SchemaRef = Arc::new(schema);
            let rows = dataset
                .rows(source, &schema)
                .ok_or_else(|| format!("no rows for {source:?}"))?;
            let batch = Batch::try_new(schema.clone(), rows).map_err(|e| format!("batch: {e}"))?;
            store = store
                .with_raw_data(source.clone(), schema, vec![batch])
                .map_err(|e| format!("bind: {e}"))?;
        }
        let root_ids: Vec<u64> = dag.roots.iter().map(|&root| root as u64).collect();
        let plan = compile(&dag, &[], &root_ids)
            .map_err(|e| format!("compile: {e}"))?
            .instantiate(Arc::new(store))
            .map_err(|e| format!("instantiate: {e}"))?;
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: EVALUATION_MS,
            },
            Limits::default(),
        )
        .map_err(|e| format!("context: {e}"))?;
        let outputs = plan
            .execute(&root_ids, context)
            .map_err(|e| format!("execute: {e}"))?;
        outputs
            .into_iter()
            .map(|stream| {
                let batches =
                    block_on(stream.try_collect::<Vec<_>>()).map_err(|e| format!("run: {e}"))?;
                Ok(batches.iter().map(|batch| batch.num_rows() as u64).sum())
            })
            .collect()
    };
    without_panics(run)
}

/// The nodes of `bounds` exported as one DAG at query time, one root each.
fn query_time_dag(
    query: &QueryBounds,
    bounds: &[usize],
) -> Result<asap_types::plan::physical_asap_dag::PhysicalASAPDAG, String> {
    let roots: Vec<QueryRoot> = bounds
        .iter()
        .map(|&bound| QueryRoot::Operator(query.bounds[bound].1.clone()))
        .collect();
    asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&roots)
        .map(|(dag, _)| dag)
        .map_err(|error| format!("export: {error:?}"))
}

/// Whether the executor compiles the nodes of `bounds` together.
fn compiles(query: &QueryBounds, bounds: &[usize]) -> Result<(), String> {
    without_panics(|| {
        let dag = query_time_dag(query, bounds)?;
        let root_ids: Vec<u64> = dag.roots.iter().map(|&root| root as u64).collect();
        compile(&dag, &[], &root_ids)
            .map(|_| ())
            .map_err(|e| format!("compile: {e}"))
    })
}

/// `run`'s result, or its panic message as an error.
fn without_panics<T>(run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
            .unwrap_or_default();
        Err(format!("panic: {message}"))
    })
}

/// A tiny deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// How one component (a label, a column, or labels declared together)
/// takes its value index over the units (series or rows) of a source,
/// by the unit's index `u`, out of `n` values.
#[derive(Clone, Copy, Debug)]
enum Pattern {
    /// `u mod n`: consecutive units differ.
    Cycle,
    /// `(u / width) mod n`: runs of `width` units share a value.
    Blocks(u64),
    /// Uniform at random.
    Random,
}

/// Where a component's values come from.
#[derive(Clone, Debug)]
enum ValueSet {
    /// Labels whose combinations the facts bound by `combinations`: each
    /// label takes value `c mod count` of combination `c`.
    Declared {
        labels: Vec<(String, u64)>,
        combinations: u64,
    },
    /// An undeclared label the query compares with literals: those values.
    Literals { label: String, values: Vec<String> },
    /// An undeclared label or column: any value; `n` of them in this
    /// dataset.
    Free { label: String },
}

#[derive(Clone, Debug)]
struct Component {
    values: ValueSet,
    /// How many values the component takes in this dataset.
    n: u64,
    pattern: Pattern,
    /// SQL: the value is the group of rows' (constant within a group)
    /// rather than each row's.
    per_group: bool,
}

/// Each source's generated data: its units, samples and components.
struct SourceData {
    units: u64,
    samples_per_unit: u64,
    /// The time between a unit's samples.
    spacing_ms: i64,
    /// The newest sample's age at evaluation time, before an anchor.
    newest_age_ms: i64,
    /// PromQL: how long before the evaluation each run of samples ends,
    /// newest first: 0, then each `offset` the query reads at.
    anchor_ages_ms: Vec<i64>,
    /// PromQL: the first half of the series exists only at the older
    /// anchors and the second half only at the newest, as when series are
    /// replaced (a restarted job).
    replaced: bool,
    components: Vec<Component>,
    counter: bool,
    non_positive_values: bool,
    is_table: bool,
}

struct Dataset {
    name: String,
    seed: u64,
    sources: BTreeMap<String, SourceData>,
    /// Each table column's or label's literal values, by name.
    literals: BTreeMap<String, Vec<String>>,
}

/// The value of index `index` of label or column `name`: its literals
/// first, then values named by index alone, so columns of different
/// tables (a join's `dst` and `host`) share values; `le` takes bucket
/// bounds.
fn value_name(name: &str, index: u64, literals: &BTreeMap<String, Vec<String>>) -> String {
    if let Some(literal) = literals
        .get(name)
        .and_then(|values| values.get(index as usize))
    {
        return literal.clone();
    }
    if name == "le" {
        return format!("{}", 1u64 << index.min(60));
    }
    format!("v{index}")
}

impl Pattern {
    fn index(self, u: u64, n: u64, rng: &mut Rng) -> u64 {
        let n = n.max(1);
        match self {
            Pattern::Cycle => u % n,
            Pattern::Blocks(width) => (u / width.max(1)) % n,
            Pattern::Random => rng.below(n),
        }
    }
}

impl Dataset {
    /// The rows of `source` in `schema`'s layout; `None` when the dataset
    /// does not describe it.
    fn rows(&self, source: &Source, schema: &SchemaRef) -> Option<Vec<Vec<Value>>> {
        let name = match source {
            Source::TimeSeries { metric } => metric.clone(),
            Source::Table { table_ref } => table_ref.clone(),
        };
        let data = self.sources.get(&name)?;
        let mut rng = Rng(self.seed ^ hash(&name));
        let mut rows = Vec::new();
        if data.is_table {
            for unit_group in 0..data.units {
                for sample in 0..data.samples_per_unit {
                    let row_index = sample * data.units + unit_group;
                    let mut cells: BTreeMap<String, Value> = BTreeMap::new();
                    for component in &data.components {
                        let u = if component.per_group {
                            unit_group
                        } else {
                            row_index
                        };
                        let index = component.pattern.index(u, component.n, &mut rng);
                        for (label, value) in self.component_values(component, index) {
                            cells.insert(label, value);
                        }
                    }
                    let time = EVALUATION_MS - data.newest_age_ms - sample as i64 * data.spacing_ms;
                    rows.push(
                        schema
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(column, field)| {
                                if Some(column) == schema.time_index {
                                    return Value::Timestamp(time);
                                }
                                let text = cells.get(&field.name);
                                table_cell(&field.dtype, text, &mut rng)
                            })
                            .collect(),
                    );
                }
            }
        } else {
            for series in 0..data.units {
                let mut labels = BTreeMap::new();
                labels.insert("__name__".to_string(), name.clone());
                labels.insert("series_index".to_string(), series.to_string());
                for component in &data.components {
                    let index = component.pattern.index(series, component.n, &mut rng);
                    for (label, value) in self.component_values(component, index) {
                        if let Value::Utf8(text) = value {
                            labels.insert(label, text.to_string());
                        }
                    }
                }
                // A replaced series exists only before the newest anchor, its
                // replacement only at it.
                let anchors = match (data.replaced, data.anchor_ages_ms.split_first()) {
                    (true, Some((newest, older))) if !older.is_empty() => {
                        if series < data.units / 2 {
                            older
                        } else {
                            std::slice::from_ref(newest)
                        }
                    }
                    _ => data.anchor_ages_ms.as_slice(),
                };
                let mut value = rng.below(1000) as f64 + 1.0;
                let times = anchors.iter().rev().flat_map(|anchor| {
                    (0..data.samples_per_unit).rev().map(move |sample| {
                        EVALUATION_MS
                            - anchor
                            - data.newest_age_ms
                            - sample as i64 * data.spacing_ms
                    })
                });
                for time in times {
                    value = if data.counter {
                        value + rng.below(100) as f64
                    } else {
                        rng.below(1000) as f64 + 1.0
                    };
                    let signed = if data.non_positive_values {
                        -value
                    } else {
                        value
                    };
                    rows.push(promql_rows::series_row(schema, &labels, time, signed).ok()?);
                }
            }
        }
        Some(rows)
    }

    /// Each label or column `component` sets, at value index `index`.
    fn component_values(&self, component: &Component, index: u64) -> Vec<(String, Value)> {
        match &component.values {
            ValueSet::Declared { labels, .. } => labels
                .iter()
                .map(|(label, count)| {
                    let text = value_name(label, index % count.max(&1), &self.literals);
                    (label.clone(), Value::Utf8(text.into()))
                })
                .collect(),
            ValueSet::Literals { label, values } => vec![(
                label.clone(),
                Value::Utf8(values[index as usize % values.len()].clone().into()),
            )],
            ValueSet::Free { label } => {
                vec![(
                    label.clone(),
                    Value::Utf8(value_name(label, index, &self.literals).into()),
                )]
            }
        }
    }
}

/// A table cell of type `dtype` from its generated text (`v{i}` or a
/// literal); a numeric cell takes the index as its number, so a declared
/// count bounds it too; with no text, a random number.
fn table_cell(dtype: &FieldDataType, text: Option<&Value>, rng: &mut Rng) -> Value {
    let index = text.and_then(|value| match value {
        Value::Utf8(text) => text.strip_prefix('v').unwrap_or(text).parse::<f64>().ok(),
        _ => None,
    });
    let number = index.unwrap_or_else(|| rng.below(1_000_000) as f64 / 7.0);
    match dtype {
        FieldDataType::Plain(DataType::Utf8) => match text {
            Some(value) => value.clone(),
            None => Value::Utf8(format!("v{}", rng.below(1_000_000)).into()),
        },
        FieldDataType::Plain(DataType::Int64) => Value::Int64(number as i64),
        FieldDataType::Plain(DataType::Float64) => Value::Float64(number),
        FieldDataType::Plain(DataType::Bool) => Value::Bool((number as u64).is_multiple_of(2)),
        FieldDataType::Plain(DataType::Timestamp) => Value::Timestamp(number as i64),
        _ => Value::Null,
    }
}

fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The labels (or columns) a query names, by source kind: every label or
/// column it compares with a literal (`m{side="l"}`, `WHERE proto =
/// 'tcp'`), with the literals, and every label its groupings and matches
/// name (`by (job)`, `on(x)`, `group_left(instance)`).
fn literals_and_named_labels(query: &str) -> (BTreeMap<String, Vec<String>>, BTreeSet<String>) {
    let chars: Vec<char> = query.chars().collect();
    let mut literals: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut named = BTreeSet::new();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        if !is_ident(chars[i]) || (i > 0 && is_ident(chars[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_ident(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let rest: String = chars[j..chars.len().min(j + 2)].iter().collect();
        let operator = ["=~", "!~", "!=", "<>", "="]
            .into_iter()
            .find(|op| rest.starts_with(op));
        if let Some(op) = operator {
            let mut k = j + op.len();
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            if let Some(&quote) = chars.get(k).filter(|c| **c == '"' || **c == '\'') {
                let end = chars[k + 1..]
                    .iter()
                    .position(|c| *c == quote)
                    .map_or(chars.len(), |p| k + 1 + p);
                let literal: String = chars[k + 1..end].iter().collect();
                if word != "__name__" {
                    let values = literals.entry(word.clone()).or_default();
                    for value in literal.split('|') {
                        if !value.is_empty() && !values.iter().any(|known| known == value) {
                            values.push(value.to_string());
                        }
                    }
                    named.insert(word.clone());
                }
            }
        }
        let grouping = [
            "by",
            "without",
            "on",
            "ignoring",
            "group_left",
            "group_right",
        ];
        if grouping.contains(&word.as_str()) && chars.get(j) == Some(&'(') {
            let end = chars[j..]
                .iter()
                .position(|c| *c == ')')
                .map_or(chars.len(), |p| j + p);
            let inside: String = chars[j + 1..end].iter().collect();
            for label in inside.split(',').map(str::trim) {
                if !label.is_empty() && label.chars().all(is_ident) {
                    named.insert(label.to_string());
                }
            }
        }
    }
    (literals, named)
}

/// When a query reads its sources, relative to its evaluation.
struct QueryTimes {
    /// The longest window it reads back from one time: its windows and
    /// subquery ranges along the longest path, plus a 5-minute lookback.
    window_ms: i64,
    /// Each time it reads at, as an age: 0, then each `offset` along a
    /// path (`m offset 1h` reads at 1 h).
    anchor_ages_ms: BTreeSet<i64>,
}

fn query_times(root: &OperatorNode) -> QueryTimes {
    fn walk(node: &OperatorNode, age_ms: i64, anchors: &mut BTreeSet<i64>) -> i64 {
        let (own_window, age_ms) = match &node.operator {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. })
            | Operator::NonASAP(NonASAPOp::PromqlSubquery { range, .. }) => {
                (range.as_millis() as i64, age_ms)
            }
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                (0, age_ms + shift.offset_ms.max(0))
            }
            _ => (0, age_ms),
        };
        anchors.insert(age_ms);
        own_window
            + node
                .children()
                .iter()
                .map(|child| walk(child, age_ms, anchors))
                .max()
                .unwrap_or(300_000)
    }
    let mut anchor_ages_ms = BTreeSet::new();
    let window_ms = walk(root, 0, &mut anchor_ages_ms);
    QueryTimes {
        window_ms,
        anchor_ages_ms,
    }
}

/// The columns each scanned source has in `root`, by source name, and
/// whether it is a table.
fn scanned_columns(root: &OperatorNode, out: &mut BTreeMap<String, (bool, BTreeSet<String>)>) {
    if let Operator::NonASAP(NonASAPOp::Scan { source, schema, .. }) = &root.operator {
        let (name, is_table) = match source {
            Source::TimeSeries { metric } => (metric.clone(), false),
            Source::Table { table_ref } => (table_ref.clone(), true),
        };
        let entry = out.entry(name).or_insert((is_table, BTreeSet::new()));
        for (column, field) in schema.fields.iter().enumerate() {
            let label = matches!(field.dtype, FieldDataType::Plain(DataType::Utf8))
                && field.name != asap_types::ir::schema::PROMQL_SERIES_IDENTITY;
            if is_table && Some(column) != schema.time_index || label {
                entry.1.insert(field.name.clone());
            }
        }
    }
    for child in root.children() {
        scanned_columns(child, out);
    }
}

/// The datasets `query` runs over: four that seek the worst case and
/// `sizes.random_datasets` random ones, each within the facts `data`
/// declares.
fn datasets(
    case: &Case,
    root: &Rc<OperatorNode>,
    data: &DataWorkload,
    sizes: DatasetSizes,
) -> Vec<Dataset> {
    let query_text: String = case
        .workload
        .query_workload
        .entries()
        .map(|entry| entry.query.0.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let (literals, named) = literals_and_named_labels(&query_text);
    let mut scanned = BTreeMap::new();
    scanned_columns(root, &mut scanned);
    let times = query_times(root);
    // Fixed worst-case seekers: declared labels random per unit, in a mixed
    // radix (every combination of the components), or all cycling
    // together; and series replaced between the times the query reads.
    let strategies = ["independent", "crossed", "aligned", "replaced"]
        .into_iter()
        .map(str::to_string)
        .chain((0..sizes.random_datasets).map(|seed| format!("random-{seed}")));
    strategies
        .enumerate()
        .map(|(index, strategy)| {
            let seed = hash(&case.name) ^ (index as u64).wrapping_mul(0x9E37_79B9);
            let mut rng = Rng(seed);
            let sources = scanned
                .iter()
                .map(|(name, (is_table, columns))| {
                    let default_facts = SourceFacts::default();
                    let facts = data.sources.get(name).unwrap_or(&default_facts);
                    let source = source_data(
                        *is_table, columns, &named, &literals, facts, &times, &strategy, sizes,
                        &mut rng,
                    );
                    (name.clone(), source)
                })
                .collect();
            Dataset {
                name: strategy,
                seed,
                sources,
                literals: literals.clone(),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn source_data(
    is_table: bool,
    columns: &BTreeSet<String>,
    named: &BTreeSet<String>,
    literals: &BTreeMap<String, Vec<String>>,
    facts: &SourceFacts,
    times: &QueryTimes,
    strategy: &str,
    sizes: DatasetSizes,
    rng: &mut Rng,
) -> SourceData {
    let random = strategy.starts_with("random");
    // Units: the declared series (groups of rows), at most `max_units`.
    let mut units = facts
        .series_count
        .unwrap_or(if random { 1 + rng.below(64) } else { 24 })
        .clamp(1, sizes.max_units);
    let interval_ms = facts
        .sample_interval
        .map_or(if is_table { 60_000 } else { 15_000 }, |interval| {
            interval.0.max(1) as i64
        });
    let mut samples_per_unit = sizes.max_samples_per_unit;
    let spacing_ms = if is_table {
        interval_ms
    } else {
        interval_ms.max(times.window_ms / (samples_per_unit as i64 - 2).max(1))
    };
    let anchor_ages_ms: Vec<i64> = if is_table {
        vec![0]
    } else {
        times.anchor_ages_ms.iter().copied().collect()
    };
    if let Some(held) = facts.rows_held {
        let runs = anchor_ages_ms.len() as u64;
        units = units.min(held / runs).max(1);
        samples_per_unit = samples_per_unit.min(held / units / runs).max(1);
    }
    let newest_age_ms = if is_table { 0 } else { interval_ms / 2 };
    // Components: labels declared together, each other declared label,
    // undeclared labels compared with literals, and the other undeclared
    // labels the query reads or names.
    let mut labels: BTreeSet<String> = columns.clone();
    if !is_table {
        labels.extend(named.iter().cloned());
        labels.extend(facts.distinct_values_per_label.keys().cloned());
        for set in &facts.distinct_values_per_label_set {
            labels.extend(set.labels.iter().cloned());
        }
    }
    let mut components = Vec::new();
    let mut in_a_set: BTreeSet<String> = BTreeSet::new();
    let mut sets: Vec<(BTreeSet<String>, u64)> = Vec::new();
    for set in &facts.distinct_values_per_label_set {
        let mut members: BTreeSet<String> = set.labels.iter().cloned().collect();
        let mut combinations = set.distinct_values;
        sets.retain(|(other, count)| {
            if other.is_disjoint(&members) {
                true
            } else {
                members.extend(other.iter().cloned());
                combinations = combinations.min(*count);
                false
            }
        });
        sets.push((members, combinations));
    }
    for (members, combinations) in sets {
        let labels = members
            .iter()
            .filter(|label| labels.contains(*label))
            .map(|label| {
                let count = facts
                    .distinct_values_per_label
                    .get(label)
                    .copied()
                    .unwrap_or(combinations);
                (label.clone(), count.min(combinations).max(1))
            })
            .collect::<Vec<_>>();
        in_a_set.extend(members);
        if !labels.is_empty() {
            components.push(ValueSet::Declared {
                labels,
                combinations: combinations.max(1),
            });
        }
    }
    for label in &labels {
        if in_a_set.contains(label) {
            continue;
        }
        components.push(match facts.distinct_values_per_label.get(label) {
            Some(&count) => ValueSet::Declared {
                labels: vec![(label.clone(), count.max(1))],
                combinations: count.max(1),
            },
            None => match literals.get(label) {
                Some(values) if !values.is_empty() => ValueSet::Literals {
                    label: label.clone(),
                    values: values.clone(),
                },
                _ => ValueSet::Free {
                    label: label.clone(),
                },
            },
        });
    }
    let total_units = if is_table {
        units * samples_per_unit
    } else {
        units
    };
    // Literal labels split the units into blocks; within a block, free
    // labels are unique, so matches across blocks pair units one to one.
    let blocks: u64 = components
        .iter()
        .map(|values| match values {
            ValueSet::Literals { values, .. } => values.len() as u64,
            ValueSet::Declared { .. } | ValueSet::Free { .. } => 1,
        })
        .product::<u64>()
        .max(1);
    let per_block = total_units.div_ceil(blocks).max(1);
    let mut block_width = total_units;
    let mut radix = 1u64;
    // Cross the smallest declared components first, so every combination
    // of the few-valued keys appears within the rows a window reads.
    components.sort_by_key(|values| match values {
        ValueSet::Declared { combinations, .. } => *combinations,
        ValueSet::Literals { .. } | ValueSet::Free { .. } => 0,
    });
    let components = components
        .into_iter()
        .map(|values| {
            let (n, pattern) = match (&values, strategy) {
                (ValueSet::Literals { values, .. }, _) if !random => {
                    let count = values.len() as u64;
                    block_width = (block_width / count).max(1);
                    (count, Pattern::Blocks(block_width))
                }
                // A replaced series and its replacement share their free
                // labels.
                (ValueSet::Free { .. }, "replaced") => ((per_block / 2).max(1), Pattern::Cycle),
                (ValueSet::Free { .. }, _) if !random => (per_block, Pattern::Cycle),
                (ValueSet::Declared { combinations, .. }, "replaced") => {
                    (*combinations, Pattern::Random)
                }
                (ValueSet::Declared { combinations, .. }, "independent") => {
                    (*combinations, Pattern::Random)
                }
                (ValueSet::Declared { combinations, .. }, "crossed") => {
                    let pattern = Pattern::Blocks(radix);
                    radix = radix.saturating_mul(*combinations);
                    (*combinations, pattern)
                }
                (ValueSet::Declared { combinations, .. }, _) if !random => {
                    (*combinations, Pattern::Cycle)
                }
                (values, _) => {
                    let n = match values {
                        ValueSet::Declared { combinations, .. } => *combinations,
                        ValueSet::Literals { values, .. } => values.len() as u64,
                        ValueSet::Free { .. } => {
                            [1, 2, 3, 10, per_block, total_units][rng.below(6) as usize]
                        }
                    };
                    let pattern = match rng.below(3) {
                        0 => Pattern::Cycle,
                        1 => Pattern::Blocks(1 + rng.below(total_units)),
                        _ => Pattern::Random,
                    };
                    (n, pattern)
                }
            };
            Component {
                values,
                n: n.max(1),
                pattern,
                per_group: is_table && random && rng.below(3) == 0,
            }
        })
        .collect();
    let value_sign = facts.column_signs.get("value");
    SourceData {
        units,
        samples_per_unit,
        spacing_ms,
        newest_age_ms,
        anchor_ages_ms,
        replaced: strategy == "replaced",
        components,
        counter: matches!(
            facts.metric_type,
            Some(asap_types::workload::PrometheusMetricType::Counter)
        ),
        non_positive_values: matches!(value_sign, Some(asap_types::stages::ValueSign::NonPositive)),
        is_table,
    }
}
