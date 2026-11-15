//! The execution oracle for value signs: a memo group's `column_signs` is a
//! claim about every non-NaN, non-NULL value the group's expression
//! computes exactly, at every evaluation time. These tests check the claim
//! against values the executor computes, never against a sign the code
//! derives itself.
//!
//! - `every_known_sign_holds_on_evaluated_values` (and its nightly 200-seed
//!   twin): every subtree node of many queries, planned alone in a memo and
//!   run exactly at query time over generated data, at several evaluation
//!   times. A column claimed `NonPositive` holding `+0.0` is a
//!   counterexample: `sum_over_time(neg[5m])` over a window of only `-0.0`
//!   evaluates to `+0.0` (`-0.0 + +0.0 = +0.0`).
//! - `exact_accumulator_realizations_keep_the_group_sign`: the same group
//!   placed at ingestion time, as a tumbling-window chain of exact states
//!   merged and finalized at query time.
//! - `a_groups_sign_does_not_depend_on_its_queries`: one expression has one
//!   sign in every memo, whatever queries share it, and no exact candidate
//!   of a group derives the opposite sign.
//!
//! The generated data satisfies every declaration: a source declared
//! `non_positive` holds only values whose sign bit is set (`-0.0`,
//! `-5e-324`, `-inf`, ...). A declaration is an assumption the planner may
//! rely on, so data that breaks it would test nothing.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use asap_devtools::planner_fuzz::generate::{random_workload, Rng};
use asap_devtools::planner_fuzz::Language;
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_stages_until, sql_catalog, RunUntil,
};
use asap_executor::physical_planner::{compile, promql_rows, test_support::views_at};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use asap_planner::{PlanningModels, PlanningWorkload, SqlCatalog};
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::stages::{Memo, MemoGroupId, ValueSign};
use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::test_support::forwarders::{
    column_signs_of_candidate_expression, physical_asap_dag_at_query_time_with_node_ids,
};
use asap_types::workload::DataWorkload;
use futures::{executor::block_on, StreamExt};
use serde_json::{json, Value as Json};

// ---------------------------------------------------------------------------
// The timeline every generated source follows, one sample per second per
// series:
//
//   (0, 21 min]       ordinary values of the declared sign; `+inf` and NaN
//                      appear only in (0, 10.5 min], so a 5-minute window
//                      ending at 21 min holds finite values only;
//   (21 min, 42 min]  only zeros of the declared sign: `-0.0` for a
//                      non-positive source, `+0.0` for a non-negative one;
//   (42 min, 63 min]  no samples at all.
//
// Evaluation times: 10.5 min (non-finite values in the window), 21 min
// (ordinary), 42 min (a window of up to 21 min holds only zeros, `-0.0`
// for `neg`), 63 min (every window of up to 21 min is empty).
// ---------------------------------------------------------------------------

const STEP_MS: i64 = 1_000;
const NON_FINITE_END_MS: i64 = 630_000;
const ORDINARY_END_MS: i64 = 1_260_000;
const ZEROS_END_MS: i64 = 2_520_000;
const EMPTY_END_MS: i64 = 3_780_000;
const SERIES: usize = 12;

const EVALUATION_TIMES: [(&str, i64); 4] = [
    ("non_finite_window", NON_FINITE_END_MS),
    ("ordinary_window", ORDINARY_END_MS),
    ("only_zeros_window", ZEROS_END_MS),
    ("empty_window", EMPTY_END_MS),
];

/// Recorded on the first run (Q12): how many (query, subtree node) pairs of
/// the CI oracle run executed at every evaluation time. Coverage below it
/// means the executor or the lowering lost operators the oracle used to
/// check. // provisional answer Q12
const MIN_COVERED_NODES_CI: usize = 1683;

/// Deterministic pseudo-random draw in `[0, 1)` from `key`.
fn unit_draw(mut key: u64) -> f64 {
    key ^= key >> 33;
    key = key.wrapping_mul(0xff51_afd7_ed55_8ccd);
    key ^= key >> 33;
    key = key.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    key ^= key >> 33;
    (key >> 11) as f64 / (1u64 << 53) as f64
}

fn name_hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// What one generated series holds, chosen from the source's facts so that
/// the data satisfies its declaration.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SeriesKind {
    /// Declared `non_negative`: values with a clear sign bit, `+0.0`,
    /// `5e-324`, `1e300`, `+inf`, ...
    NonNegative,
    /// Declared `non_positive`: the mirror image, `-0.0`, `-5e-324`,
    /// `-inf`, ...
    NonPositive,
    /// Undeclared: both, plus series crossing zero and series alternating
    /// `+0.0` and `-0.0`.
    Mixed,
    /// An undeclared or non-negative counter: monotone, resets to `+0.0`.
    Counter,
    /// An undeclared histogram `_sum`: decreasing, the running sum of
    /// negative observations (a latency offset, a balance change).
    DecreasingSum,
    /// The SQL `p` column: values in `[-1, 1]` of the declared sign.
    Fraction(Declared),
    /// The SQL `bytes` Int64 column: `0`, `1` or `5000`.
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Declared {
    NonNegative,
    NonPositive,
    Undeclared,
}

/// The non-negative pattern of series `s` at sample `k` (draw `r`). The
/// special values come first: `+0.0`, the smallest subnormal, tiny and huge
/// magnitudes, and `1e11`, a time before year 1 when negated.
fn non_negative_value(s: usize, k: u64, t_ms: i64, r: f64) -> f64 {
    const FINITE: [f64; 7] = [0.0, 5e-324, 1e-300, 0.3, 1.0, 1e300, 1e11];
    const NON_FINITE: [f64; 2] = [f64::INFINITY, f64::NAN];
    let pareto = (1.0 - r).powf(-1.0 / 0.8) * (1 + s % 7) as f64;
    match s {
        // Every special value in turn; +inf and NaN only early on.
        0 if t_ms <= NON_FINITE_END_MS && k % 9 >= 7 => NON_FINITE[(k % 9 - 7) as usize],
        0 => FINITE[(k % 7) as usize],
        1 => 0.0,
        2 => r * 1000.0,
        3 => pareto,
        4 => [0.0, 5e-324, 1e-300][(k % 3) as usize],
        5 => [1e300, 1.0, 0.3][(k % 3) as usize],
        6 if t_ms <= NON_FINITE_END_MS && k % 41 == 0 => f64::INFINITY,
        6 => FINITE[((k + 3) % 7) as usize],
        7 if k % 37 == 0 => FINITE[((k / 37) % 7) as usize],
        7 => r * 100.0,
        8 => (r * 10.0).floor() / 10.0,
        9 if r < 0.05 => 0.0,
        9 => pareto,
        10 => r,
        _ => (r * 5.0).floor(),
    }
}

/// The values of one series of `kind` over the timeline: `(t_ms, value)`.
fn series_values(kind: SeriesKind, s: usize, key: u64) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let mut counter = 0.0f64;
    let mut t_ms = STEP_MS;
    let mut k = 0u64;
    while t_ms <= ZEROS_END_MS {
        let r = unit_draw(key ^ (s as u64) << 40 ^ k.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let zeros = t_ms > ORDINARY_END_MS;
        let value = match kind {
            SeriesKind::NonNegative if zeros => 0.0,
            SeriesKind::NonNegative => non_negative_value(s, k, t_ms, r),
            SeriesKind::NonPositive if zeros => -0.0,
            SeriesKind::NonPositive => -non_negative_value(s, k, t_ms, r),
            SeriesKind::Mixed if zeros => [0.0, -0.0][s % 2],
            SeriesKind::Mixed => match s % 4 {
                0 => non_negative_value(s, k, t_ms, r),
                1 => -non_negative_value(s, k, t_ms, r),
                2 => (r - 0.5) * 100.0,
                _ => [0.0, -0.0][(k % 2) as usize],
            },
            SeriesKind::Counter => {
                if zeros || s == 1 || k % (150 + 17 * s as u64) == 0 {
                    // A reset (a restart) to +0.0; flat at +0.0 later on.
                    counter = 0.0;
                } else {
                    counter += (r * 10.0 * (1 + s % 5) as f64).floor();
                }
                counter
            }
            SeriesKind::DecreasingSum => {
                counter -= r * 10.0;
                counter
            }
            SeriesKind::Fraction(declared) => {
                let magnitude = if zeros || k % 23 == 0 { 0.0 } else { r };
                match declared {
                    Declared::NonNegative => magnitude,
                    Declared::NonPositive => -magnitude,
                    Declared::Undeclared if zeros => [0.0, -0.0][s % 2],
                    Declared::Undeclared => 2.0 * r - 1.0,
                }
            }
            SeriesKind::Bytes if zeros => 0.0,
            SeriesKind::Bytes => [0.0, 1.0, 5000.0][((k + s as u64) % 3) as usize],
        };
        out.push((t_ms, value));
        t_ms += STEP_MS;
        k += 1;
    }
    out
}

fn declared_sign(facts: &Json, column: &str) -> Declared {
    match facts["column_signs"][column].as_str() {
        Some("non_negative") => Declared::NonNegative,
        Some("non_positive") => Declared::NonPositive,
        _ => Declared::Undeclared,
    }
}

fn gauge_kind(declared: Declared) -> SeriesKind {
    match declared {
        Declared::NonNegative => SeriesKind::NonNegative,
        Declared::NonPositive => SeriesKind::NonPositive,
        Declared::Undeclared => SeriesKind::Mixed,
    }
}

/// The kind of the series of PromQL metric `metric`, from its facts (or
/// its histogram family's): a counter declared `non_positive` holds
/// non-positive values, as its declaration says. // provisional answer Q8
fn metric_kind(sources: &Json, metric: &str) -> SeriesKind {
    if let Some(facts) = sources.get(metric) {
        let declared = declared_sign(facts, "value");
        return match (facts["metric_type"].as_str(), declared) {
            (Some("counter"), Declared::NonPositive) => SeriesKind::NonPositive,
            (Some("counter"), _) => SeriesKind::Counter,
            _ => gauge_kind(declared),
        };
    }
    for suffix in ["_sum", "_count", "_bucket"] {
        let Some(family) = metric.strip_suffix(suffix) else {
            continue;
        };
        let Some(facts) = sources.get(family) else {
            continue;
        };
        // "Declared" is the family's `value` entry. // provisional answer Q8
        return match (suffix, declared_sign(facts, "value")) {
            (_, Declared::NonPositive) => SeriesKind::NonPositive,
            ("_sum", Declared::Undeclared) => SeriesKind::DecreasingSum,
            _ => SeriesKind::Counter,
        };
    }
    SeriesKind::Mixed
}

/// How the generated series are labelled: the review workloads match
/// `job="j0"`, the fuzz generator `job="job1"`.
#[derive(Debug, Clone, Copy)]
enum Labels {
    Review,
    Fuzz,
}

fn series_labels(scheme: Labels, metric: &str, s: usize) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([("__name__".to_string(), metric.to_string())]);
    let pairs: Vec<(&str, String)> = match scheme {
        Labels::Review => vec![
            ("instance", format!("i{s}")),
            ("job", format!("j{}", s % 3)),
            ("host", format!("h{}", s % 4)),
            ("region", format!("r{}", s % 2)),
        ],
        Labels::Fuzz => vec![
            ("job", format!("job{}", s % 5 + 1)),
            ("instance", format!("instance{}", s + 1)),
            ("region", format!("region{}", s % 3 + 1)),
        ],
    };
    labels.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    labels
}

/// The value of a SQL text column `column` in "series" `s`.
fn sql_text(column: &str, s: usize) -> String {
    match column {
        "src" => format!("s{}", s % 6),
        "dst" => format!("d{s}"),
        _ => format!("{column}{}", s + 1),
    }
}

/// The union of the schemas `dag`'s `Scan`s read from each source, by
/// column name.
fn scanned_schemas(dag: &PhysicalASAPDAG) -> BTreeMap<Source, Schema> {
    let mut schemas: BTreeMap<Source, Schema> = BTreeMap::new();
    for node in &dag.nodes {
        let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, schema, .. }) =
            &node.payload
        else {
            continue;
        };
        let known = schemas
            .entry(source.clone())
            .or_insert_with(|| schema.clone());
        for field in &schema.fields {
            if !known.fields.iter().any(|f| f.name == field.name) {
                known.fields.push(field.clone());
            }
        }
    }
    schemas
}

/// The data of one oracle case: its facts, label scheme and seed.
struct CaseData {
    facts: DataWorkload,
    sources: Json,
    labels: Labels,
    seed: u64,
    /// The generated raw data per source and scanned schema: queries of one
    /// case scan the same sources with the same columns.
    batches: std::cell::RefCell<HashMap<(Source, String), Batch>>,
}

impl CaseData {
    fn new(facts: Json, labels: Labels, seed: u64) -> Self {
        let sources = facts["sources"].clone();
        Self {
            facts: serde_json::from_value(facts).expect("facts deserialize"),
            sources,
            labels,
            seed,
            batches: Default::default(),
        }
    }

    /// The raw data every `Scan` of `dag` reads, generated per declared
    /// sign.
    fn store_for(&self, dag: &PhysicalASAPDAG) -> Result<InMemoryDataStore, String> {
        let mut store = InMemoryDataStore::default();
        for (source, schema) in scanned_schemas(dag) {
            let key = (source.clone(), format!("{schema:?}"));
            let schema = Arc::new(schema);
            let cached = self.batches.borrow().get(&key).cloned();
            let batch = match cached {
                Some(batch) => batch,
                None => {
                    let rows = match &source {
                        Source::TimeSeries { metric } => self.time_series_rows(&schema, metric)?,
                        Source::Table { table_ref } => self.table_rows(&schema, table_ref)?,
                    };
                    let batch = Batch::try_new(schema.clone(), rows).map_err(|e| e.to_string())?;
                    self.batches.borrow_mut().insert(key, batch.clone());
                    batch
                }
            };
            store = store
                .with_raw_data(source, schema, vec![batch])
                .map_err(|e| e.to_string())?;
        }
        Ok(store)
    }

    fn time_series_rows(
        &self,
        schema: &Arc<Schema>,
        metric: &str,
    ) -> Result<Vec<Vec<Value>>, String> {
        let kind = metric_kind(&self.sources, metric);
        let mut rows = Vec::new();
        for s in 0..SERIES {
            let labels = series_labels(self.labels, metric, s);
            for (t_ms, value) in series_values(kind, s, self.seed ^ name_hash(metric)) {
                rows.push(
                    promql_rows::series_row(schema, &labels, t_ms, value)
                        .map_err(|e| e.to_string())?,
                );
            }
        }
        Ok(rows)
    }

    fn table_rows(&self, schema: &Arc<Schema>, table: &str) -> Result<Vec<Vec<Value>>, String> {
        let facts = &self.sources[table];
        let mut columns = Vec::new();
        for (index, field) in schema.fields.iter().enumerate() {
            let generated: Option<Vec<(i64, f64)>> = match &field.dtype {
                _ if Some(index) == schema.time_index => None,
                FieldDataType::Plain(DataType::Int64) => Some(Vec::new()),
                FieldDataType::Plain(DataType::Float64) => Some(Vec::new()),
                FieldDataType::Plain(DataType::Utf8) => None,
                other => {
                    return Err(format!(
                        "no generated data for {table}.{}: {other:?}",
                        field.name
                    ))
                }
            };
            columns.push(generated);
        }
        let mut rows = Vec::new();
        for s in 0..SERIES {
            let per_column: Vec<Option<Vec<(i64, f64)>>> = schema
                .fields
                .iter()
                .zip(&columns)
                .map(|(field, numeric)| {
                    numeric.as_ref().map(|_| {
                        let declared = declared_sign(facts, &field.name);
                        let kind = match (&field.dtype, field.name.as_str()) {
                            (FieldDataType::Plain(DataType::Int64), _) => SeriesKind::Bytes,
                            (_, "p") => SeriesKind::Fraction(declared),
                            _ => gauge_kind(declared),
                        };
                        series_values(kind, s, self.seed ^ name_hash(&field.name))
                    })
                })
                .collect();
            let steps = (ZEROS_END_MS / STEP_MS) as usize;
            for step in 0..steps {
                let t_ms = (step as i64 + 1) * STEP_MS;
                let row = schema
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(index, field)| {
                        if Some(index) == schema.time_index {
                            return Value::Timestamp(t_ms);
                        }
                        match (&field.dtype, &per_column[index]) {
                            (FieldDataType::Plain(DataType::Int64), Some(values)) => {
                                Value::Int64(values[step].1 as i64)
                            }
                            (_, Some(values)) => Value::Float64(values[step].1),
                            _ => Value::Utf8(sql_text(&field.name, s).into()),
                        }
                    })
                    .collect();
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// The oracle.
// ---------------------------------------------------------------------------

/// One value that breaks its column's claimed sign.
#[derive(Debug)]
struct Counterexample {
    case: String,
    query: String,
    node: String,
    column: String,
    evaluation: String,
    row: String,
    value: String,
    derived: ValueSign,
}

#[derive(Default)]
struct OracleReport {
    counterexamples: Vec<Counterexample>,
    /// `(case, query, node kind, reason)` of every node or query the
    /// executor or the lowering cannot run.
    skipped: Vec<(String, String, String, String)>,
    /// `(node kind, column, observed sign) -> (count, an example query)`:
    /// Unknown columns whose evaluated values all had one sign bit.
    precision: BTreeMap<(String, String, &'static str), (usize, String)>,
    covered_nodes: usize,
    checked_values: usize,
}

impl OracleReport {
    fn print(&self, title: &str) {
        println!("==== value sign oracle: {title} ====");
        println!(
            "covered nodes: {}, checked values: {}, counterexamples: {}, skipped: {}",
            self.covered_nodes,
            self.checked_values,
            self.counterexamples.len(),
            self.skipped.len()
        );
        // Every counterexample, one line per (case, query, node, column,
        // evaluation time, value class), with how many values it stands for
        // and the first one: a window of 12 series breaking a sign at once
        // is one line, not twelve.
        let mut grouped: BTreeMap<
            (&str, &str, &str, &str, &str, String, &str),
            (usize, &Counterexample),
        > = BTreeMap::new();
        for c in &self.counterexamples {
            let class = match c.value.as_str() {
                "0.0" | "-0.0" => c.value.as_str(),
                v if v.starts_with('-') => "negative",
                _ => "positive",
            };
            let entry = grouped
                .entry((
                    &c.case,
                    &c.query,
                    &c.node,
                    &c.column,
                    &c.evaluation,
                    format!("{:?}", c.derived),
                    class,
                ))
                .or_insert((0, c));
            entry.0 += 1;
        }
        println!(
            "---- counterexamples ({} values, {} lines) ----",
            self.counterexamples.len(),
            grouped.len()
        );
        for ((case, query, node, column, evaluation, derived, class), (count, first)) in &grouped {
            println!(
                "COUNTEREXAMPLE x{count} case={case} query={query:?} node={node} column={column} \
                 eval={evaluation} derived={derived} values={class} first={} row=[{}]",
                first.value, first.row
            );
        }
        let mut by_reason: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();
        for (case, query, node, reason) in &self.skipped {
            let entry = by_reason
                .entry((node.clone(), reason.chars().take(160).collect()))
                .or_insert((0, format!("{case}: {query}")));
            entry.0 += 1;
        }
        println!(
            "---- skipped ({} total, {} kinds) ----",
            self.skipped.len(),
            by_reason.len()
        );
        for ((node, reason), (count, example)) in &by_reason {
            println!("SKIPPED x{count} node={node} reason={reason} e.g. {example}");
        }
        println!(
            "---- precision: Unknown columns with values of one sign ({}) ----",
            self.precision.len()
        );
        for ((node, column, sign), (count, example)) in &self.precision {
            println!(
                "PRECISION x{count} node={node} column={column} observed={sign} e.g. {example}"
            );
        }
    }
}

/// The operator kind of `payload`: `Aggregate`, `TimeShift`, ...
fn operator_kind(payload: &PhysicalASAPOperatorPayload) -> String {
    let text = format!("{payload:?}");
    let inner = text.split_once('(').map(|(_, rest)| rest).unwrap_or(&text);
    inner
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or(inner)
        .to_string()
}

/// A short readable rendering of `payload` for a report line.
fn operator_text(payload: &PhysicalASAPOperatorPayload) -> String {
    let text = format!("{payload:?}");
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(240).collect()
}

/// A row's text cells, for locating a counterexample's series.
fn row_text(schema: &Schema, row: &[Value]) -> String {
    schema
        .fields
        .iter()
        .zip(row)
        .filter_map(|(field, cell)| match cell {
            Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => Some(
                promql_rows::decode_series_identity(text)
                    .map(|labels| format!("{labels:?}"))
                    .unwrap_or_else(|_| text.to_string()),
            ),
            Value::Utf8(text) if !text.is_empty() => Some(format!("{}={text}", field.name)),
            Value::Timestamp(t) => Some(format!("{}@{t}", field.name)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A numeric cell as the value whose sign bit it carries: an Int64 `v`
/// counts as `v as f64`, so int `0` is `+0.0`. NULL, NaN and non-numeric
/// cells carry no claim (`None`). // provisional answer Q5, Q6
fn numeric(cell: &Value) -> Option<f64> {
    match cell {
        Value::Float64(v) if v.is_nan() => None,
        Value::Float64(v) => Some(*v),
        Value::Int64(v) => Some(*v as f64),
        _ => None,
    }
}

fn run_node(
    dag: &PhysicalASAPDAG,
    store: &Arc<InMemoryDataStore>,
    id: usize,
    evaluation_time_ms: i64,
) -> Result<Vec<Batch>, String> {
    let roots = [id as u64];
    let compiled = compile(dag, &[], &roots).map_err(|e| format!("compile: {e}"))?;
    let bound = compiled
        .instantiate(store.clone())
        .map_err(|e| format!("instantiate: {e}"))?;
    let context = RunContext::new(Scope::Query { evaluation_time_ms }, Limits::default())
        .map_err(|e| format!("context: {e}"))?;
    block_on(async {
        let mut output = bound
            .execute(&roots, context)
            .map_err(|e| format!("execute: {e}"))?
            .remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.map_err(|e| format!("stream: {e}"))?).clone());
        }
        Ok(batches)
    })
}

/// The signs the memo derives for `node` planned alone: the root group's
/// schema field names and `column_signs`.
fn derived_signs(
    node: &Rc<OperatorNode>,
    facts: &DataWorkload,
) -> Result<(Vec<String>, Vec<ValueSign>), String> {
    let memo =
        asap_stage0_memo::memo_of_queries(&[QueryRoot::Operator(node.clone())], &[None], facts)
            .map_err(|e| format!("memo: {e}"))?;
    let QueryRoot::Operator(group) = &memo.queries()[0].root else {
        return Err("memo: scalar root".into());
    };
    let properties = &memo.groups()[group.0 as usize].group_properties;
    Ok((
        properties
            .schema
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect(),
        properties.column_signs.clone(),
    ))
}

/// Check every subtree node of `root` (query `query` of case `case`)
/// against the values the executor computes at every evaluation time.
fn check_root(
    case: &str,
    query: &str,
    root: &QueryRoot,
    data: &CaseData,
    report: &mut OracleReport,
) {
    let skip = |report: &mut OracleReport, node: &str, reason: String| {
        report.skipped.push((
            case.to_string(),
            query.to_string(),
            node.to_string(),
            reason,
        ))
    };
    let (dag, nodes) =
        match physical_asap_dag_at_query_time_with_node_ids(std::slice::from_ref(root)) {
            Ok(exported) => exported,
            Err(e) => return skip(report, "<query>", format!("export: {e}")),
        };
    let store = match data.store_for(&dag) {
        Ok(store) => Arc::new(store),
        Err(e) => return skip(report, "<query>", format!("data: {e}")),
    };
    for node in &dag.nodes {
        let kind = operator_kind(&node.payload);
        let (names, signs) = match derived_signs(&nodes[node.id], &data.facts) {
            Ok(derived) => derived,
            Err(e) => {
                skip(report, &kind, e);
                continue;
            }
        };
        let output_names: Vec<String> = node
            .output_schema
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();
        if names != output_names || signs.len() != names.len() {
            skip(
                report,
                &kind,
                format!(
                    "schema: group {names:?} with {} signs, node {output_names:?}",
                    signs.len()
                ),
            );
            continue;
        }
        // Per column: the sign bits seen, for the precision list.
        let mut seen_negative = vec![false; signs.len()];
        let mut seen_positive = vec![false; signs.len()];
        let mut ran_everywhere = true;
        // A raw `Scan` returns the same rows at every time: run it once.
        let scan = matches!(
            node.payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
        );
        let times = if scan {
            &EVALUATION_TIMES[..1]
        } else {
            &EVALUATION_TIMES[..]
        };
        for &(evaluation, time_ms) in times {
            let batches = match run_node(&dag, &store, node.id, time_ms) {
                Ok(batches) => batches,
                Err(e) => {
                    skip(report, &kind, e);
                    ran_everywhere = false;
                    break;
                }
            };
            for batch in &batches {
                for row in batch.to_rows() {
                    for (column, cell) in row.iter().enumerate() {
                        let Some(value) = numeric(cell) else { continue };
                        report.checked_values += 1;
                        let negative = value.is_sign_negative();
                        seen_negative[column] |= negative;
                        seen_positive[column] |= !negative;
                        let holds = match signs[column] {
                            ValueSign::NonNegative => !negative,
                            ValueSign::NonPositive => negative,
                            ValueSign::Unknown => true,
                        };
                        if !holds {
                            report.counterexamples.push(Counterexample {
                                case: case.to_string(),
                                query: query.to_string(),
                                node: format!("#{} {}", node.id, operator_text(&node.payload)),
                                column: names[column].clone(),
                                evaluation: format!("{evaluation}@{time_ms}ms"),
                                row: row_text(batch.schema(), &row),
                                value: format!("{value:?}"),
                                derived: signs[column],
                            });
                        }
                    }
                }
            }
        }
        if !ran_everywhere {
            continue;
        }
        report.covered_nodes += 1;
        for (column, sign) in signs.iter().enumerate() {
            if *sign != ValueSign::Unknown || seen_negative[column] == seen_positive[column] {
                continue;
            }
            let observed = if seen_negative[column] {
                "all sign bit set"
            } else {
                "all sign bit clear"
            };
            let entry = report
                .precision
                .entry((kind.clone(), names[column].clone(), observed))
                .or_insert((0, format!("{case}: {query}")));
            entry.0 += 1;
        }
    }
}

/// One query of an oracle case, as a single-query workload with no
/// accuracy requirement: the exact plan.
fn single_query_workload(language: &Json, query: &str, facts: &Json) -> PlanningWorkload {
    serde_json::from_value(json!({
        "query_workload": {
            "language": language,
            "repeating_queries": [{
                "query": query,
                "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                "requirements": {},
                "predictability": {"predictable": {"known_at": null}},
            }],
        },
        "data_workload": facts,
    }))
    .expect("a single-query workload deserializes")
}

/// An oracle case: queries in one language over one set of facts.
struct OracleCase {
    name: String,
    language: Language,
    /// SQL only: the catalog the queries are lowered over.
    catalog: Option<SqlCatalog>,
    /// `(workload name, query text)`.
    queries: Vec<(String, String)>,
    facts: Json,
    labels: Labels,
    seed: u64,
}

fn run_case(case: &OracleCase, report: &mut OracleReport) {
    let data = CaseData::new(case.facts.clone(), case.labels, case.seed);
    let language = match case.language {
        Language::Promql => json!("promql"),
        Language::Sql => json!({"sql": "datafusion_sql"}),
    };
    for (workload_name, query) in &case.queries {
        let case_name = format!("{}/{workload_name}", case.name);
        let workload = single_query_workload(&language, query, &case.facts);
        // The production lowering: PromQL with each series' identity as a
        // column, as planning lowers it.
        let lowered = match (&case.language, &case.catalog) {
            (Language::Promql, _) => lower_promql_roots(&workload),
            (Language::Sql, Some(catalog)) => lower_sql_roots(&workload, catalog),
            (Language::Sql, None) => Err("no SQL catalog".into()),
        };
        match lowered {
            Ok(roots) => {
                for root in &roots {
                    check_root(&case_name, query, root, &data, report);
                }
            }
            Err(e) => report.skipped.push((
                case_name,
                query.clone(),
                "<query>".into(),
                format!("lowering: {e}"),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// The fixed adversarial list: the sign review workloads, inlined with their
// facts (series counts and sample intervals normalized to the generated
// data: 12 series, one sample per second).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Facts {
    /// `data` N, `neg` P, `mixed` U, `data_total` counter, `negc` counter
    /// declared P.
    Review,
    /// `data` declared P.
    DataNonPositive,
    /// `data` undeclared.
    DataUndeclared,
    /// `flows`: `bytes` Int64 declared N, `p` undeclared.
    FlowsBytesNonNegative,
    /// `flows`: `bytes` N and `p` N.
    FlowsBytesAndPNonNegative,
    /// `flows`: `p` N, `bytes` undeclared.
    FlowsPNonNegative,
    /// `flows`: `bytes` N, `p` declared P (values in `(-1, -0.0]`).
    FlowsPNonPositive,
    /// This plan's sources: `pos` N, `neg` P, `mixed` U, `c_total` counter,
    /// `foo` histogram family, all undeclared but `pos`/`neg`.
    Findings,
}

fn gauge(sign: Option<&str>) -> Json {
    let mut facts = json!({"series_count": 12, "metric_type": "gauge", "sample_interval": 1000});
    if let Some(sign) = sign {
        facts["column_signs"] = json!({"value": sign});
    }
    facts
}

fn counter(sign: Option<&str>) -> Json {
    let mut facts = gauge(sign);
    facts["metric_type"] = json!("counter");
    facts
}

fn flows(bytes: Option<&str>, p: Option<&str>) -> Json {
    let mut signs = serde_json::Map::new();
    if let Some(sign) = bytes {
        signs.insert("bytes".into(), json!(sign));
    }
    if let Some(sign) = p {
        signs.insert("p".into(), json!(sign));
    }
    json!({"flows": {
        "rows_held": 36000,
        "distinct_values_per_label": {"src": 6, "dst": 300},
        "column_signs": signs,
    }})
}

impl Facts {
    fn json(self) -> Json {
        let n = Some("non_negative");
        let p = Some("non_positive");
        let sources = match self {
            Facts::Review => json!({
                "data": gauge(n), "neg": gauge(p), "mixed": gauge(None),
                "data_total": counter(None), "negc": counter(p),
            }),
            Facts::DataNonPositive => json!({"data": gauge(p)}),
            Facts::DataUndeclared => json!({"data": gauge(None)}),
            Facts::FlowsBytesNonNegative => flows(n, None),
            Facts::FlowsBytesAndPNonNegative => flows(n, n),
            Facts::FlowsPNonNegative => flows(None, n),
            Facts::FlowsPNonPositive => flows(n, p),
            Facts::Findings => json!({
                "pos": gauge(n), "neg": gauge(p), "mixed": gauge(None),
                "c_total": counter(None),
                "foo": {"series_count": 12, "metric_type": "histogram", "sample_interval": 1000},
            }),
        };
        let rows_per_second = if self.is_sql() { 12.0 } else { 60.0 };
        json!({"arrival": {"ingesting": {"rows_per_second": rows_per_second}}, "sources": sources})
    }

    fn is_sql(self) -> bool {
        matches!(
            self,
            Facts::FlowsBytesNonNegative
                | Facts::FlowsBytesAndPNonNegative
                | Facts::FlowsPNonNegative
                | Facts::FlowsPNonPositive
        )
    }
}

/// The review workloads' SQL table: `bytes` is an Int64 column, so integer
/// zeros (`-0` is `0`) are exercised.
fn flows_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("src", DataType::Utf8, false),
            Field::plain("dst", DataType::Utf8, false),
            Field::plain("bytes", DataType::Int64, false),
            Field::plain("p", DataType::Float64, false),
        ],
        0,
        vec![],
    )
}

/// The sign review workloads: r1302 `s01`–`s40`, `x01`–`x07`; r1309
/// `e01`–`e11` and `a`, `b`, `c`, `d`, `i`, `k`.
const REVIEW_QUERIES: &[(&str, Facts, &str)] = &[
    ("r1302/s01_q_gt1", Facts::Review, "avg by (job) (quantile_over_time(1.5, data[5m]))"),
    ("r1302/s02_q_neg", Facts::Review, "avg by (job) (quantile_over_time(-0.5, data[5m]))"),
    ("r1302/s03_q_nan", Facts::Review, "avg by (job) (quantile_over_time(NaN, data[5m]))"),
    ("r1302/s04_rate_counter", Facts::Review, "sum by (job) (rate(data_total[5m]))"),
    ("r1302/s05_increase_counter", Facts::Review, "sum by (job) (increase(data_total[5m]))"),
    ("r1302/s06_rate_neg_gauge", Facts::Review, "sum by (job) (rate(neg[5m]))"),
    ("r1302/s07_irate_gauge", Facts::Review, "avg by (job) (irate(data[5m]))"),
    ("r1302/s08_deriv", Facts::Review, "avg by (job) (deriv(data[5m]))"),
    ("r1302/s09_delta", Facts::Review, "avg by (job) (delta(data[5m]))"),
    ("r1302/s10_sub", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) - quantile_over_time(0.5, data[5m]))"),
    ("r1302/s11_neg", Facts::Review, "avg by (job) (-quantile_over_time(0.9, data[5m]))"),
    ("r1302/s12_negneg_mixed", Facts::Review, "avg by (job) (-(-quantile_over_time(0.9, mixed[5m])))"),
    ("r1302/s13_abs_mixed", Facts::Review, "avg by (job) (abs(quantile_over_time(0.9, mixed[5m])))"),
    ("r1302/s14_clamp", Facts::Review, "avg by (job) (clamp(quantile_over_time(0.9, data[5m]), -5, 5))"),
    ("r1302/s15_clamp_min0_mixed", Facts::Review, "avg by (job) (clamp_min(quantile_over_time(0.9, mixed[5m]), 0))"),
    ("r1302/s16_times_neg_const", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) * -2)"),
    ("r1302/s17_neg_const_times", Facts::Review, "avg by (job) (-2 * quantile_over_time(0.9, data[5m]))"),
    ("r1302/s18_const_minus", Facts::Review, "avg by (job) (100 - quantile_over_time(0.9, data[5m]))"),
    ("r1302/s19_scalar_cmp_left_mixed", Facts::Review, "avg by (job) (100 > quantile_over_time(0.9, mixed[5m]))"),
    ("r1302/s20_cmp_bool", Facts::Review, "avg by (job) (quantile_over_time(0.9, mixed[5m]) > bool 1)"),
    ("r1302/s21_nonpos_source", Facts::Review, "avg by (job) (quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s22_or_union", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) or quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s23_add_mixed_signs", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) + on(instance, job, host, region) quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s24_div_signs", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) / on(instance, job, host, region) quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s24m_div_noacc", Facts::Review, "sum by (job) (sum_over_time(data[5m]) / on(instance, job, host, region) sum_over_time(neg[5m]))"),
    ("r1302/s24n_div_noacc", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) / on(instance, job, host, region) quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s25_unless", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) unless quantile_over_time(0.9, neg[5m]))"),
    ("r1302/s26_relabel", Facts::Review, "avg by (job) (label_replace(quantile_over_time(0.9, data[5m]), \"x\", \"$1\", \"instance\", \"(.*)\"))"),
    ("r1302/s27_subquery_deriv", Facts::Review, "avg by (job) (quantile_over_time(0.9, deriv(data[1m])[5m:30s]))"),
    ("r1302/s28_pow", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) ^ 3)"),
    ("r1302/s29_scalar_mul", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) * scalar(min(neg)))"),
    ("r1302/s30_mod", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) % 7)"),
    ("r1302/s31_rate_times_q", Facts::Review, "sum by (job) (rate(data_total[5m]) * on(instance, job, host, region) quantile_over_time(0.9, data[5m]))"),
    ("r1302/s32_stddev", Facts::Review, "sum by (job) (stddev_over_time(data[5m]))"),
    ("r1302/s33_sgn_mixed", Facts::Review, "avg by (job) (sgn(quantile_over_time(0.9, mixed[5m])))"),
    ("r1302/s34_offset", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m] offset 1m))"),
    ("r1302/s35_group_right", Facts::Review, "quantile_over_time(0.9, neg[5m]) * on(job) group_right sum by (job) (data)"),
    ("r1302/s36_sum_of_group_right", Facts::Review, "sum by (job) (quantile_over_time(0.9, neg[5m]) * on(instance) group_right quantile_over_time(0.9, data[5m]))"),
    ("r1302/s37_sum_without", Facts::Review, "sum without (instance, host) (quantile_over_time(0.9, data[5m]))"),
    ("r1302/s38_vector_neg", Facts::Review, "sum(vector(-1))"),
    ("r1302/s39_minover_mixed_abs", Facts::Review, "avg by (job) (abs(min_over_time(mixed[5m])))"),
    ("r1302/s40_histq", Facts::Review, "sum(histogram_quantile(0.9, rate(data_total[5m])))"),
    ("r1302/x01_sql_case", Facts::FlowsBytesAndPNonNegative, "SELECT src, SUM(CASE WHEN p > 0.5 THEN bytes ELSE -bytes END) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x02_sql_avg", Facts::FlowsBytesNonNegative, "SELECT src, AVG(bytes) AS a FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x03_sql_neg", Facts::FlowsBytesNonNegative, "SELECT src, SUM(bytes * -1) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x04_sql_abs", Facts::FlowsBytesNonNegative, "SELECT src, SUM(ABS(bytes - 5000)) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x05_sql_sub", Facts::FlowsBytesAndPNonNegative, "SELECT src, AVG(bytes - p) AS a FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x06_sql_cast", Facts::FlowsBytesNonNegative, "SELECT src, SUM(CAST(bytes - 1 AS BIGINT)) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1302/x07_sql_negp", Facts::FlowsPNonNegative, "SELECT src, SUM(-p) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    ("r1309/e01_quantile_grouped", Facts::Review, "avg by (region) (quantile_over_time(0.9, data[5m]))"),
    ("r1309/e02_rm_long_w01_q3", Facts::Review, "avg by (region) (quantile_over_time(0.95, data[300s]))"),
    ("r1309/e03_rm_w01_q3_q8#0", Facts::Review, "avg by (region) (quantile_over_time(0.95, data[100s]))"),
    ("r1309/e03_rm_w01_q3_q8#1", Facts::Review, "topk by (region) (4, rate(data_total[20m]))"),
    ("r1309/e03a_avgq_alone", Facts::Review, "avg by (region) (quantile_over_time(0.95, data[100s]))"),
    ("r1309/e03b_topk_rate_alone", Facts::Review, "topk by (region) (4, rate(data_total[20m]))"),
    ("r1309/e04_rm_w02_q2_q7#0", Facts::Review, "avg by (region) (quantile_over_time(0.75, data[100s]))"),
    ("r1309/e04_rm_w02_q2_q7#1", Facts::Review, "topk by (region) (4, increase(data_total[5m]))"),
    ("r1309/e04a_alone", Facts::Review, "avg by (region) (quantile_over_time(0.75, data[100s]))"),
    ("r1309/e04b_alone", Facts::Review, "topk by (region) (4, increase(data_total[5m]))"),
    ("r1309/e05_example_avg", Facts::Review, "avg by (job) (quantile_over_time(0.99, data[300s]))"),
    ("r1309/e06_example_sum", Facts::Review, "sum by (job) (quantile_over_time(0.99, data[300s]))"),
    ("r1309/e07_example_unknown", Facts::DataUndeclared, "avg by (job) (quantile_over_time(0.99, data[300s]))"),
    ("r1309/e08_example_nonpos", Facts::DataNonPositive, "avg by (job) (quantile_over_time(0.99, data[300s]))"),
    ("r1309/e09_example_sum_nonpos", Facts::DataNonPositive, "sum by (job) (quantile_over_time(0.5, data[300s]))"),
    ("r1309/e10_example_avg_all", Facts::Review, "avg(quantile_over_time(0.5, data[300s]))"),
    ("r1309/e11_example_sum_neg", Facts::Review, "sum by (job) (-quantile_over_time(0.9, data[300s]))"),
    ("r1309/a01_and_subset", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) and quantile_over_time(0.9, neg{job!=\"j0\"}[5m]))"),
    ("r1309/a02_unless_subset", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) unless quantile_over_time(0.9, neg{job=\"j0\"}[5m]))"),
    ("r1309/a03_or_subset_left", Facts::Review, "avg by (job) (quantile_over_time(0.9, data{job=\"j0\"}[5m]) or quantile_over_time(0.5, data[5m]))"),
    ("r1309/a03b_or_same_series", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) or quantile_over_time(0.5, data[5m]))"),
    ("r1309/a03c_or_disjoint_metrics", Facts::Review, "max by (job) (quantile_over_time(0.9, data[5m]) or quantile_over_time(0.9, neg[5m]))"),
    ("r1309/a04_and_on", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) and on(job) quantile_over_time(0.9, neg{instance=\"i0\"}[5m]))"),
    ("r1309/a05_unless_ignoring", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) unless ignoring(instance, host, region) quantile_over_time(0.9, neg{job=\"j1\"}[5m]))"),
    ("r1309/a06_and_on_agg_right", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) and on(job) (avg by (job) (quantile_over_time(0.9, neg[5m]))))"),
    ("r1309/a07_unless_on_value", Facts::Review, "max by (job) (quantile_over_time(0.9, data[5m]) unless on(value) quantile_over_time(0.9, neg[5m]))"),
    ("r1309/a07b_and_on_value", Facts::Review, "max by (job) (quantile_over_time(0.9, data[5m]) and on(value) quantile_over_time(0.9, data[5m]))"),
    ("r1309/a07c_unless_on_avg", Facts::Review, "max by (job) (quantile_over_time(0.9, data[5m]) unless on(avg) avg by (job) (quantile_over_time(0.9, neg[5m])))"),
    ("r1309/a08_relabel_src_value", Facts::Review, "avg by (job) (label_replace(quantile_over_time(0.9, data[5m]), \"x\", \"$1\", \"value\", \"(.*)\"))"),
    ("r1309/a09_relabel_dst_value", Facts::Review, "avg by (job) (label_replace(quantile_over_time(0.9, data[5m]), \"value\", \"z\", \"job\", \"(.*)\"))"),
    ("r1309/a10_relabel_group_by_dst", Facts::Review, "avg by (x) (label_replace(quantile_over_time(0.9, data[5m]), \"x\", \"$1\", \"job\", \"(.*)\"))"),
    ("r1309/a11_label_join", Facts::Review, "avg by (job) (label_join(quantile_over_time(0.9, data[5m]), \"x\", \"-\", \"job\", \"instance\"))"),
    ("r1309/a12_relabel_src_avg", Facts::Review, "max(label_replace(avg by (job) (quantile_over_time(0.9, data[5m])), \"x\", \"$1\", \"avg\", \"(.*)\"))"),
    ("r1309/a12b_relabel_bare", Facts::Review, "label_replace(quantile_over_time(0.9, data[5m]), \"x\", \"$1\", \"job\", \"(.*)\")"),
    ("r1309/a13_abs_cross_zero_avg", Facts::Review, "avg by (job) (abs(quantile_over_time(0.3, mixed[5m])))"),
    ("r1309/a14_abs_cross_zero", Facts::Review, "abs(quantile_over_time(0.3, mixed[5m]))"),
    ("r1309/a15_neg_cross_zero", Facts::Review, "-quantile_over_time(0.3, mixed[5m])"),
    ("r1309/a16_sum_neg_unknown", Facts::Review, "sum by (job) (-quantile_over_time(0.3, mixed[5m]))"),
    ("r1309/a17_unless_then_sum", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) unless on(job) quantile_over_time(0.9, neg{job=\"j2\"}[5m]))"),
    ("r1309/a18_or_then_sum_mixed_sign", Facts::Review, "sum by (job) (quantile_over_time(0.9, data{job=\"j0\"}[5m]) or quantile_over_time(0.9, neg[5m]))"),
    ("r1309/a19_and_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) and quantile_over_time(0.9, neg{job!=\"j1\"}[5m])"),
    ("r1309/a20_or_bare_same", Facts::Review, "quantile_over_time(0.9, data[5m]) or quantile_over_time(0.1, data[5m])"),
    ("r1309/b01_and_subset_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) and quantile_over_time(0.9, neg{job!=\"j0\"}[5m])"),
    ("r1309/b02_unless_subset_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) unless quantile_over_time(0.9, neg{job=\"j0\"}[5m])"),
    ("r1309/b02g_unless_full_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) unless quantile_over_time(0.9, neg[5m])"),
    ("r1309/b02h_and_full_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) and quantile_over_time(0.9, neg[5m])"),
    ("r1309/b05_unless_ignoring_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) unless ignoring(instance, host, region) quantile_over_time(0.9, neg{job=\"j1\"}[5m])"),
    ("r1309/b06_and_on_agg_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) and on(job) avg by (job) (quantile_over_time(0.9, neg[5m]))"),
    ("r1309/b07c_unless_on_avg_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) unless on(avg) avg by (job) (quantile_over_time(0.9, neg[5m]))"),
    ("r1309/b17_unless_on_job_bare", Facts::Review, "quantile_over_time(0.9, data[5m]) unless on(job) quantile_over_time(0.9, neg{job=\"j2\"}[5m])"),
    ("r1309/b20_or_gappy_both", Facts::Review, "quantile_over_time(0.9, data[5m]) or quantile_over_time(0.9, mixed[5m])"),
    ("r1309/b21_or_same_metric_q", Facts::Review, "quantile_over_time(0.5, neg[5m]) or quantile_over_time(0.9, neg[5m])"),
    ("r1309/b22_avg_and_on_instance", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) and on(instance) quantile_over_time(0.9, neg[5m]))"),
    ("r1309/b23_avg_unless_on_instance", Facts::Review, "avg by (job) (quantile_over_time(0.9, data[5m]) unless on(instance) quantile_over_time(0.9, neg[5m]))"),
    ("r1309/b24_sum_or_on_instance", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) or on(instance) quantile_over_time(0.5, data[5m]))"),
    ("r1309/c01_q03_mixed_bare", Facts::Review, "quantile_over_time(0.3, mixed[5m])"),
    ("r1309/c02_q03_data_bare", Facts::Review, "quantile_over_time(0.3, data[5m])"),
    ("r1309/d01_and_exact_right", Facts::Review, "quantile_over_time(0.3, mixed[5m]) and max_over_time(neg[5m])"),
    ("r1309/d02_and_sketch_right", Facts::Review, "quantile_over_time(0.3, mixed[5m]) and quantile_over_time(0.9, neg[5m])"),
    ("r1309/i01_sum_over_time_match", Facts::Review, "sum by (job) (sum_over_time(data{job!=\"j2\"}[5m]))"),
    ("r1309/i02_sum_over_time", Facts::Review, "sum_over_time(data[5m])"),
    ("r1309/i03_count_over_time_match", Facts::Review, "count_over_time(data{job=~\"j0|j1\"}[5m])"),
    ("r1309/i04_max_over_time_match", Facts::Review, "max_over_time(data{job=\"j1\"}[5m])"),
    ("r1309/i05_sum_instant", Facts::Review, "sum by (job) (data)"),
    ("r1309/i06_max_by", Facts::Review, "max by (job) (data)"),
    ("r1309/i07_sum_of_max_over_time", Facts::Review, "sum by (job) (max_over_time(data[5m]))"),
    ("r1309/i08_count_by", Facts::Review, "count by (job) (data{job!=\"j0\"})"),
    ("r1309/i09_div_sum", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) / on(instance, job, host, region) quantile_over_time(0.9, neg[5m]))"),
    ("r1309/i09b_div_sum_acc", Facts::Review, "sum by (job) (quantile_over_time(0.9, data[5m]) / on(instance, job, host, region) quantile_over_time(0.9, neg[5m]))"),
    ("r1309/i10_avg_over_time", Facts::Review, "avg_over_time(data{job=\"j0\"}[5m])"),
    ("r1309/i11_min_over_time", Facts::Review, "min_over_time(data[5m])"),
    ("r1309/k01_rate_negc", Facts::Review, "rate(negc[5m])"),
    ("r1309/k02_sum_rate_negc", Facts::Review, "sum by (job) (rate(negc[5m]))"),
    ("r1309/k03_increase_negc", Facts::Review, "increase(negc[5m])"),
    ("r1309/k04_avg_rate_negc_acc", Facts::Review, "avg by (job) (rate(negc[5m]))"),
];

/// The new queries for this plan's findings, over [`Facts::Findings`] (or
/// the `flows` table for SQL).
const FINDING_QUERIES: &[(&str, Facts, &str)] = &[
    // F1: a sum or average of -0.0 values. // provisional answer Q1
    ("F1", Facts::Findings, "sum_over_time(neg[5m])"),
    ("F1", Facts::Findings, "avg_over_time(neg[5m])"),
    ("F1", Facts::Findings, "sum by (job) (neg)"),
    // F2: sgn(-0.0). // provisional answer Q2
    ("F2", Facts::Findings, "sgn(neg)"),
    // F3: clamps at signed zeros. // provisional answer Q3
    ("F3", Facts::Findings, "clamp_min(neg, 0)"),
    ("F3", Facts::Findings, "clamp_max(pos, -0)"),
    ("F3", Facts::Findings, "clamp(mixed, 0, -0)"),
    // F4: year of a time before year 1 (`-1e11` s). // provisional answer Q4
    ("F4", Facts::Findings, "year(neg)"),
    // F5: integer zeros carry no sign bit. // provisional answer Q5
    ("F5", Facts::FlowsBytesNonNegative, "SELECT -bytes AS v FROM flows WHERE ts >= now() - INTERVAL '5 minutes'"),
    ("F5", Facts::FlowsBytesNonNegative, "SELECT bytes * -1 AS v FROM flows WHERE ts >= now() - INTERVAL '5 minutes'"),
    ("F5", Facts::FlowsBytesNonNegative, "SELECT bytes / -2 AS v FROM flows WHERE ts >= now() - INTERVAL '5 minutes'"),
    ("F5", Facts::FlowsBytesNonNegative, "SELECT src, -COUNT(p) AS c FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    // B14: an undeclared histogram `_sum` of negative observations
    // decreases, so its rate is negative. // provisional answer Q8
    ("B14", Facts::Findings, "rate(foo_sum[5m])"),
    // B15: a cast to an integer of values in (-1, 0) gives int 0.
    // // provisional answer Q5
    ("B15", Facts::FlowsPNonPositive, "SELECT CAST(p AS BIGINT) AS c FROM flows WHERE ts >= now() - INTERVAL '5 minutes'"),
    ("B15", Facts::FlowsPNonPositive, "SELECT src, SUM(CAST(p AS BIGINT)) AS s FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY src"),
    // A vector/vector `bool` comparison returns 0 or 1 (`+0.0` for false),
    // whatever its left side's sign.
    ("bool", Facts::Findings, "neg > bool neg"),
    ("bool", Facts::Findings, "neg > bool (neg * 2)"),
    // I6: evaluation contexts copy their input's sign.
    ("I6", Facts::Findings, "neg offset 1m"),
    ("I6", Facts::Findings, "neg @ 100"),
    ("I6", Facts::Findings, "neg[5m:30s]"),
    ("I6", Facts::Findings, "sum_over_time(neg[5m:30s])"),
];

fn fixed_cases() -> Vec<OracleCase> {
    let mut by_facts: BTreeMap<String, OracleCase> = BTreeMap::new();
    for (name, facts, query) in REVIEW_QUERIES.iter().chain(FINDING_QUERIES) {
        let key = format!("{facts:?}");
        let case = by_facts.entry(key.clone()).or_insert_with(|| OracleCase {
            name: format!("fixed/{key}"),
            language: if facts.is_sql() {
                Language::Sql
            } else {
                Language::Promql
            },
            catalog: facts
                .is_sql()
                .then(|| SqlCatalog::new().with_table("flows", flows_schema())),
            queries: Vec::new(),
            facts: facts.json(),
            labels: Labels::Review,
            seed: 1,
        });
        case.queries.push((name.to_string(), query.to_string()));
    }
    by_facts.into_values().collect()
}

/// The three source-sign variants of a fuzz workload: as generated (every
/// source declared `non_negative`), every source declared
/// `non_positive`, and every source undeclared.
fn fuzz_cases(seeds: std::ops::Range<u64>) -> Vec<OracleCase> {
    let mut cases = Vec::new();
    for seed in seeds {
        let workload = random_workload(&mut Rng::new(seed));
        let facts = serde_json::to_value(&workload.data).expect("facts serialize");
        let tables: Vec<String> = workload.tables.iter().map(Json::to_string).collect();
        for variant in ["as_generated", "non_positive", "undeclared"] {
            let mut facts = facts.clone();
            for source in facts["sources"]
                .as_object_mut()
                .into_iter()
                .flat_map(|s| s.values_mut())
            {
                let source = source.as_object_mut().expect("source facts are an object");
                match variant {
                    "non_positive" => {
                        source.insert("column_signs".into(), json!({"value": "non_positive"}));
                    }
                    "undeclared" => {
                        source.remove("column_signs");
                    }
                    _ => {}
                }
            }
            let catalog = match workload.language {
                Language::Promql => None,
                Language::Sql => Some(sql_catalog(&tables).expect("generated tables parse").0),
            };
            cases.push(OracleCase {
                name: format!("fuzz/seed{seed}/{variant}"),
                language: workload.language,
                catalog,
                queries: workload
                    .queries
                    .iter()
                    .enumerate()
                    .map(|(i, q)| (format!("q{i}"), q.text().to_string()))
                    .collect(),
                facts,
                labels: Labels::Fuzz,
                seed,
            });
        }
    }
    cases
}

fn run_oracle(cases: &[OracleCase], title: &str) -> OracleReport {
    let mut report = OracleReport::default();
    let started = std::time::Instant::now();
    for case in cases {
        run_case(case, &mut report);
        eprintln!(
            "oracle: {} done at {:.1} s, {} covered nodes",
            case.name,
            started.elapsed().as_secs_f64(),
            report.covered_nodes
        );
    }
    report.print(title);
    report
}

/// T3.1. Every known sign holds on every value the executor computes for
/// the node's group, over the fixed adversarial list, this plan's new
/// queries, and 50 fuzz seeds in three source-sign variants. The expected
/// signs come only from the evaluated values. // provisional answer Q12
#[test]
fn every_known_sign_holds_on_evaluated_values() {
    let mut cases = fixed_cases();
    cases.extend(fuzz_cases(0..50));
    let report = run_oracle(&cases, "fixed list + 50 fuzz seeds x 3 sign variants");
    assert!(
        report.covered_nodes >= MIN_COVERED_NODES_CI,
        "covered nodes dropped: {} < {MIN_COVERED_NODES_CI}",
        report.covered_nodes
    );
    assert!(
        report.counterexamples.is_empty(),
        "{} values break their column's derived sign (listed above)",
        report.counterexamples.len()
    );
}

/// T3.1 over 200 fuzz seeds, run nightly. // provisional answer Q12
#[test]
#[ignore = "nightly"]
fn every_known_sign_holds_on_evaluated_values_over_200_fuzz_seeds() {
    let report = run_oracle(&fuzz_cases(0..200), "200 fuzz seeds x 3 sign variants");
    assert!(
        report.counterexamples.is_empty(),
        "{} values break their column's derived sign (listed above)",
        report.counterexamples.len()
    );
}

// ---------------------------------------------------------------------------
// T3.2: the ingestion-time placement of one exact group.
// ---------------------------------------------------------------------------

const MINUTE_MS: i64 = 60_000;
/// The chain's pane width and the query's window.
const PANE_MS: i64 = MINUTE_MS;
const WINDOW_MS: i64 = 5 * MINUTE_MS;

fn chain_samples_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ],
        1,
        vec![],
    )
}

fn shared(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).expect("a valid operator")
}

/// `Finalize(SummaryMerge(TimeRange(5m, SummaryTumblingWindowChain(kind
/// per job of metric's value, 1-minute panes))))`: the exact `kind` of each
/// job's values in the last 5 minutes, as an ingestion-time chain computes
/// it. Returns the finalize node and the chain node.
fn finalized_chain(metric: &str, kind: ExactKind) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
    let params = match kind {
        ExactKind::Sum => ExactParams::Sum,
        ExactKind::Count => ExactParams::Count,
        ExactKind::Min => ExactParams::Min,
        ExactKind::Max => ExactParams::Max,
        ExactKind::Increase => ExactParams::Increase,
        ExactKind::Rate => ExactParams::Rate,
        ExactKind::IRate => ExactParams::IRate,
    };
    let scan = shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: chain_samples_schema(),
    }));
    let chain = shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan,
        summary_type: FieldDataType::ExactAggregate(kind, params),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
        tumbling_window_width_ms: PANE_MS as u64,
    }));
    let range = shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(WINDOW_MS as u64),
        kind: TimeRangeKind::Range,
        child: chain.clone(),
    }));
    let merge = shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![range],
        group_by: None,
    }));
    let finalize = shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: merge,
    }));
    (finalize, chain)
}

/// The samples of `metric` (`pos`: non-negative, `neg`: non-positive), one
/// every 10 s per job over `(0, 30 min]`: ordinary values up to 20 min,
/// then only zeros of the declared sign (`-0.0` for `neg`), with no sample
/// at all in `(27 min, 28 min]`, an empty pane inside the window ending at
/// 30 min.
fn chain_samples(metric: &str) -> Vec<(String, i64, f64)> {
    let mut samples = Vec::new();
    for job in 0..3usize {
        let mut t = 10_000;
        let mut k = 0u64;
        while t <= 30 * MINUTE_MS {
            let empty_pane = t > 27 * MINUTE_MS && t <= 28 * MINUTE_MS;
            if !empty_pane {
                let r = unit_draw(name_hash(metric) ^ (job as u64) << 32 ^ k);
                // The finite special values only: a NaN or inf state would
                // hide the zeros' sign.
                let magnitude = if t > 20 * MINUTE_MS {
                    0.0
                } else {
                    match job {
                        0 => [0.0, 5e-324, 1e-300, 0.3, 1.0, 1e300][(k % 6) as usize],
                        1 => 0.0,
                        _ => r * 100.0,
                    }
                };
                let value = if metric == "neg" {
                    -magnitude
                } else {
                    magnitude
                };
                samples.push((format!("j{job}"), t, value));
            }
            t += 10_000;
            k += 1;
        }
    }
    samples
}

fn chain_store(metric: &str) -> InMemoryDataStore {
    let schema = Arc::new(chain_samples_schema());
    let rows = chain_samples(metric)
        .into_iter()
        .map(|(job, t, value)| {
            vec![
                Value::Utf8(job.into()),
                Value::Timestamp(t),
                Value::Float64(value),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    InMemoryDataStore::default()
        .with_raw_data(
            Source::TimeSeries {
                metric: metric.into(),
            },
            schema,
            vec![batch],
        )
        .unwrap()
}

/// Run `ids` of `dag` once, every output's rows.
fn run_ids(
    dag: &asap_executor::plan::PhysicalDAG<'static, Batch, asap_executor::values::SchemaRef>,
    ids: &[u64],
    context: RunContext,
) -> Result<Vec<Vec<Vec<Value>>>, String> {
    block_on(async {
        let mut all = Vec::new();
        for mut output in dag.execute(ids, context).map_err(|e| e.to_string())? {
            let mut rows = Vec::new();
            while let Some(batch) = output.next().await {
                rows.extend(batch.map_err(|e| e.to_string())?.to_rows());
            }
            all.push(rows);
        }
        Ok(all)
    })
}

/// The finalized values of `kind` over `metric` at each evaluation time,
/// from a chain built at ingestion pane by pane, appended to the store and
/// merged at query time: `(evaluation time, job, value)`.
fn chain_answers(
    metric: &str,
    kind: ExactKind,
    times_ms: &[i64],
) -> Result<Vec<(i64, String, Value)>, String> {
    let (finalize, chain) = finalized_chain(metric, kind);
    let (mut dag, nodes) =
        physical_asap_dag_at_query_time_with_node_ids(&[QueryRoot::Operator(finalize.clone())])
            .map_err(|e| e.to_string())?;
    let id_of = |node: &Rc<OperatorNode>| nodes.iter().position(|n| Rc::ptr_eq(n, node)).unwrap();
    let (chain_id, finalize_id) = (id_of(&chain), id_of(&finalize));
    dag.nodes
        .iter_mut()
        .find(|node| node.id == chain_id)
        .unwrap()
        .retention_ms = Some(60 * MINUTE_MS as u64);
    let views = views_at(&mut dag, &[chain_id as u64]).map_err(|e| format!("views: {e}"))?;
    let refresh =
        compile(&dag, &[], &[chain_id as u64]).map_err(|e| format!("compile chain: {e}"))?;
    let query =
        compile(&dag, &views, &[finalize_id as u64]).map_err(|e| format!("compile query: {e}"))?;
    let chain_schema = Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id == chain_id)
            .unwrap()
            .output_schema
            .clone(),
    );
    let mut store = Arc::new(chain_store(metric));
    let mut window_end = PANE_MS;
    while window_end <= 30 * MINUTE_MS {
        let context = RunContext::new(
            Scope::Ingestion {
                window_start_ms: window_end - PANE_MS,
                window_end_ms: window_end,
            },
            Limits::default(),
        )
        .unwrap();
        let rows = run_ids(
            &refresh
                .instantiate(store.clone())
                .map_err(|e| e.to_string())?,
            &[chain_id as u64],
            context,
        )?
        .remove(0);
        let batch = Batch::try_new(chain_schema.clone(), rows).map_err(|e| e.to_string())?;
        Arc::get_mut(&mut store)
            .unwrap()
            .append_materialized_view(&views[0], (window_end - PANE_MS, window_end), vec![batch])
            .map_err(|e| format!("append: {e}"))?;
        window_end += PANE_MS;
    }
    let mut answers = Vec::new();
    for &evaluation_time_ms in times_ms {
        let context =
            RunContext::new(Scope::Query { evaluation_time_ms }, Limits::default()).unwrap();
        let rows = run_ids(
            &query
                .instantiate(store.clone())
                .map_err(|e| e.to_string())?,
            &[finalize_id as u64],
            context,
        )?
        .remove(0);
        for row in rows {
            let job = row
                .iter()
                .find_map(|cell| match cell {
                    Value::Utf8(job) => Some(job.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            for cell in row {
                if matches!(cell, Value::Float64(_) | Value::Int64(_)) {
                    answers.push((evaluation_time_ms, job.clone(), cell));
                }
            }
        }
    }
    Ok(answers)
}

/// T3.2. One exact group placed at ingestion time: a chain of exact states
/// in 1-minute panes, merged over 5 minutes and finalized at query time.
/// Every finalized value satisfies the finalize group's sign. At 30 min
/// the window of `neg` holds only `-0.0` and one empty pane: an exact Sum
/// state starts at `+0.0`, so a merged `-0.0` total turns into `+0.0`,
/// against `NonPositive` (F1). // provisional answer Q1
#[test]
fn exact_accumulator_realizations_keep_the_group_sign() {
    let facts: DataWorkload = serde_json::from_value(json!({
        "arrival": {"ingesting": {"rows_per_second": 0.6}},
        "sources": {"pos": gauge(Some("non_negative")), "neg": gauge(Some("non_positive"))},
    }))
    .unwrap();
    let kinds = [
        ExactKind::Sum,
        ExactKind::Min,
        ExactKind::Max,
        ExactKind::Count,
        ExactKind::Rate,
        ExactKind::Increase,
    ];
    let mut counterexamples = Vec::new();
    let mut not_run = Vec::new();
    let mut checked = 0usize;
    for metric in ["pos", "neg"] {
        for kind in kinds {
            let (finalize, _) = finalized_chain(metric, kind);
            let finalize_node_signs = match derived_signs(&finalize, &facts) {
                Ok((names, signs)) => {
                    println!("{metric} {kind:?}: group columns {names:?} signs {signs:?}");
                    signs
                }
                Err(e) => {
                    not_run.push(format!("{metric} {kind:?}: {e}"));
                    continue;
                }
            };
            // The answers' numeric columns are the group's numeric columns.
            let numeric_signs: Vec<ValueSign> = finalize
                .schema
                .fields
                .iter()
                .zip(&finalize_node_signs)
                .filter(|(field, _)| {
                    matches!(
                        field.dtype,
                        FieldDataType::Plain(DataType::Float64 | DataType::Int64)
                    )
                })
                .map(|(_, sign)| *sign)
                .collect();
            let answers = match chain_answers(metric, kind, &[20 * MINUTE_MS, 30 * MINUTE_MS]) {
                Ok(answers) => answers,
                Err(e) => {
                    not_run.push(format!("{metric} {kind:?}: {e}"));
                    continue;
                }
            };
            let [sign] = numeric_signs[..] else {
                not_run.push(format!(
                    "{metric} {kind:?}: numeric columns {numeric_signs:?}"
                ));
                continue;
            };
            for (time_ms, job, cell) in answers {
                let Some(value) = numeric(&cell) else {
                    continue;
                };
                checked += 1;
                let holds = match sign {
                    ValueSign::NonNegative => !value.is_sign_negative(),
                    ValueSign::NonPositive => value.is_sign_negative(),
                    ValueSign::Unknown => true,
                };
                if !holds {
                    counterexamples.push(format!(
                        "{metric} {kind:?} at {time_ms} ms, job {job}: {value:?} against {sign:?}"
                    ));
                }
            }
        }
    }
    println!("checked {checked} finalized values; not run: {not_run:#?}");
    for c in &counterexamples {
        println!("COUNTEREXAMPLE {c}");
    }
    assert!(
        not_run.is_empty(),
        "realizations that did not run: {not_run:#?}"
    );
    assert!(checked > 0, "no finalized value was checked");
    assert!(counterexamples.is_empty(), "{counterexamples:#?}");
}

// ---------------------------------------------------------------------------
// T3.3: one group, one sign, whatever queries share it.
// ---------------------------------------------------------------------------

/// `group`'s expression with every input group replaced by its own
/// rendering: equal across memos exactly when the expressions are.
fn rendered(memo: &Memo, group: MemoGroupId, cache: &mut HashMap<MemoGroupId, String>) -> String {
    if let Some(text) = cache.get(&group) {
        return text.clone();
    }
    let expression = &memo.groups()[group.0 as usize].expression;
    let children: Vec<MemoGroupId> = expression.children().into_iter().copied().collect();
    let child_texts: HashMap<MemoGroupId, String> = children
        .iter()
        .map(|child| (*child, rendered(memo, *child, cache)))
        .collect();
    let text = format!(
        "{:?}",
        expression.map_children(|child| child_texts[child].clone())
    );
    cache.insert(group, text.clone());
    text
}

/// Whether two signs contradict: one says `NonNegative`, the other
/// `NonPositive`. `Unknown` contradicts nothing.
fn contradicts(a: ValueSign, b: ValueSign) -> bool {
    matches!(
        (a, b),
        (ValueSign::NonNegative, ValueSign::NonPositive)
            | (ValueSign::NonPositive, ValueSign::NonNegative)
    )
}

/// T3.3. A group's sign is derived once, from its expression: the same
/// expression has the same `column_signs` in every memo, whichever queries
/// share it and in whatever order they arrive, after Stage 1 has added its
/// rewrite candidates (finer-aggregate reuse for `q2` over `q1`). And no
/// exact candidate of a group derives a sign that contradicts the group's.
/// // provisional answer Q11
#[test]
fn a_groups_sign_does_not_depend_on_its_queries() {
    let q1 = "sum by (job, host) (rate(c_total[5m]))";
    let q2 = "sum by (job) (rate(c_total[5m]))";
    let q3 = "sum(-quantile_over_time(0.9, pos[5m]))";
    let q4 = "sum(-quantile_over_time(0.9, pos[5m])) unless neg";
    let facts = Facts::Findings.json();
    let data: DataWorkload = serde_json::from_value(facts.clone()).unwrap();
    let capabilities = asap_planner::DeploymentCapabilities {
        supported_summaries: Some(asap_executor::supported_summary_families()),
        ..asap_planner::DeploymentCapabilities::default()
    };
    let mut memos: Vec<(String, Memo)> = Vec::new();
    for queries in [vec![q1, q2], vec![q2, q1], vec![q2], vec![q3, q4], vec![q4]] {
        let workload: PlanningWorkload = serde_json::from_value(json!({
            "query_workload": {
                "language": "promql",
                "repeating_queries": queries.iter().map(|query| json!({
                    "query": query,
                    "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
                    "requirements": {"accuracy": {
                        "max_error": 0.05, "failure_probability": 0.3,
                        "accuracy_scope": "whole_result", "accuracy_metric": "relative",
                    }},
                    "predictability": {"predictable": {"known_at": null}},
                })).collect::<Vec<_>>(),
            },
            "data_workload": facts,
        }))
        .unwrap();
        let roots = lower_promql_roots(&workload).unwrap();
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
        let run = run_stages_until(roots, &entries, &data, models, RunUntil::Pass2).unwrap();
        let label = queries.join(" ; ");
        memos.push((
            format!("[{label}] Stage 0"),
            run.stage0_memo_groups.clone().expect("Stage 0 ran"),
        ));
        let pass2 = run.run.logical_asap_candidates.unwrap_or_else(|| {
            panic!(
                "[{label}]: Pass 2 did not run: {:?}",
                run.run.selection.err()
            )
        });
        memos.push((format!("[{label}] Pass 2"), pass2.memo));
    }
    // The same expression, the same signs, in every memo holding it.
    let mut signs_of: BTreeMap<String, Vec<(String, Vec<ValueSign>)>> = BTreeMap::new();
    for (label, memo) in &memos {
        let mut cache = HashMap::new();
        for index in 0..memo.groups().len() {
            let group = MemoGroupId(index as u32);
            signs_of
                .entry(rendered(memo, group, &mut cache))
                .or_default()
                .push((
                    label.clone(),
                    memo.groups()[index].group_properties.column_signs.clone(),
                ));
        }
    }
    let shared_expressions = signs_of.values().filter(|memos| memos.len() > 1).count();
    let disagreements: Vec<String> = signs_of
        .iter()
        .filter(|(_, signs)| signs.iter().any(|(_, s)| *s != signs[0].1))
        .map(|(expression, signs)| {
            format!(
                "{}: {signs:?}",
                expression.chars().take(300).collect::<String>()
            )
        })
        .collect();
    println!(
        "{} distinct expressions, {shared_expressions} in more than one memo, {} disagree",
        signs_of.len(),
        disagreements.len()
    );
    for d in &disagreements {
        println!("DISAGREEMENT {d}");
    }
    assert!(
        shared_expressions > 0,
        "no expression is shared across memos"
    );
    // Every exact candidate's own signs never contradict its group's. A
    // sketch build or readout computes estimates, which `column_signs` does
    // not cover, so only exact candidates are compared.
    let mut contradictions = Vec::new();
    let mut checked_candidates = 0usize;
    let mut candidate_errors = Vec::new();
    for (label, memo) in memos.iter().filter(|(label, _)| label.ends_with("Pass 2")) {
        for group in memo.groups() {
            for candidate in &group.candidates {
                let operator = &candidate.expression.operator;
                let estimate = matches!(operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. }));
                if estimate || candidate.logical_properties.parameters().is_some() {
                    continue;
                }
                let group_signs = &group.group_properties.column_signs;
                match column_signs_of_candidate_expression(memo, operator) {
                    Ok(signs) => {
                        checked_candidates += 1;
                        let contradicting = signs.len() != group_signs.len()
                            || signs
                                .iter()
                                .zip(group_signs)
                                .any(|(c, g)| contradicts(*c, *g));
                        if contradicting {
                            contradictions.push(format!(
                                "{label}: candidate {signs:?} vs group {group_signs:?}: {}",
                                format!("{operator:?}")
                                    .chars()
                                    .take(240)
                                    .collect::<String>()
                            ));
                        }
                    }
                    Err(e) => candidate_errors.push(format!("{label}: {e}")),
                }
            }
        }
    }
    println!("checked {checked_candidates} exact candidates");
    for c in &contradictions {
        println!("CONTRADICTION {c}");
    }
    assert!(disagreements.is_empty(), "{disagreements:#?}");
    assert!(candidate_errors.is_empty(), "{candidate_errors:#?}");
    assert!(checked_candidates > 0, "no exact candidate was checked");
    assert!(contradictions.is_empty(), "{contradictions:#?}");
}
