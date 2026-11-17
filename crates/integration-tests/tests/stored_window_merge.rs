//! `sum by (job) (rate(requests_total[1h]))` evaluated every 60 s over
//! counters sampled at 10 Hz, through the plan the Planner selects: a chain
//! of 1-minute counter windows built at ingestion, its last 60 windows
//! merged per series at each evaluation. Its answers equal, bit for bit,
//! the answers recorded with the hash merge (`stored_window_merge_fixtures/`),
//! so a faster merge of the stored windows answers as the hash merge did.
//! The data has what changes a merge's input: counter resets, a series that
//! ends (a stale marker) and one that starts within the hour.
//!
//! `UPDATE_SNAPSHOTS=1` re-records the answers; only a commit whose merge
//! is the hash merge may record them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use asap_executor::physical_planner::compile;
use asap_executor::physical_planner::promql_rows::encode_series_identity;
use asap_executor::runtime::{Limits, OutputStream, RunContext, Scope};
use asap_executor::sources::{DataStore, MaterializedViewRequest, RawDataRequest};
use asap_executor::values::{Batch, SchemaRef, Value};
use asap_executor::Error;
use asap_planner::{FrontendInput, PlanningModels, PlanningWorkload, UserInput};
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, FieldDataType, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::plan::materialized_view::MaterializedView;
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::summary_type::ExactKind;
use futures::{executor::block_on, stream, StreamExt};

const QUERY: &str = "sum by (job) (rate(requests_total[1h]))";
const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;
/// 10 Hz.
const SAMPLE_INTERVAL_MS: i64 = 100;
const SERIES: usize = 12;
/// The evaluations compared: three, one minute apart, each over the hour
/// before it.
const EVALUATIONS_MS: [i64; 3] = [
    HOUR_MS + MINUTE_MS,
    HOUR_MS + 2 * MINUTE_MS,
    HOUR_MS + 3 * MINUTE_MS,
];
/// Series 10's last sample, followed by a stale marker: it ends.
const ENDS_MS: i64 = 25 * MINUTE_MS;
/// Series 11's first sample: it starts.
const STARTS_MS: i64 = 35 * MINUTE_MS + 30 * SECOND_MS;
/// The stale marker's bits: the sample that ends a PromQL series.
const STALE_MARKER: f64 = f64::from_bits(0x7ff0_0000_0000_0002);

fn workload() -> PlanningWorkload {
    serde_json::from_value(serde_json::json!({
        "query_workload": {
            "language": "promql",
            "repeating_queries": [{
                "query": QUERY,
                "demand": {"fixed_interval_at": {"interval": MINUTE_MS, "evaluation_phase": 0}},
                "requirements": {"accuracy": {
                    "max_error": 0.01,
                    "failure_probability": 0.01,
                    "accuracy_scope": "per_value",
                    "accuracy_metric": "relative"
                }},
                "predictability": {"predictable": {"known_at": null}}
            }]
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": SERIES as f64 * 10.0}},
            "sources": {"requests_total": {
                "series_count": SERIES,
                "metric_type": "counter",
                "sample_interval": SAMPLE_INTERVAL_MS
            }}
        }
    }))
    .unwrap()
}

/// The plan the Planner selects for the workload.
fn selected_plan() -> PhysicalASAPDAG {
    let workload = workload();
    let (plan, _) = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(asap_planner::plan_installable(UserInput::new(
            &workload,
            FrontendInput::Promql,
            PlanningModels::builtin(),
        )))
        .unwrap();
    plan.dag
}

/// The test reaches the merge it guards only through a plan that merges
/// stored counter windows: a chain of 1-minute rate windows, read by a
/// `SummaryMerge`.
fn assert_merges_stored_counter_windows(dag: &PhysicalASAPDAG) {
    let chain = dag.nodes.iter().find_map(|node| match &node.payload {
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type,
            tumbling_window_width_ms,
            ..
        }) => Some((summary_type.clone(), *tumbling_window_width_ms)),
        _ => None,
    });
    assert_eq!(
        chain,
        Some((
            FieldDataType::ExactAggregate(
                ExactKind::Rate,
                asap_types::summary_type::ExactParams::Rate
            ),
            MINUTE_MS as u64
        )),
        "the selected plan no longer stores 1-minute rate windows; re-record \
         the answers on a commit with the hash merge"
    );
    assert!(dag.nodes.iter().any(|node| matches!(
        node.payload,
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryMerge { .. })
    )));
}

/// Series `series`' labels: `instance` `i<n>`, `job` `j<n mod 4>`.
fn labels(series: usize) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("__name__".to_string(), "requests_total".to_string()),
        ("instance".to_string(), format!("i{series:02}")),
        ("job".to_string(), format!("j{}", series % 4)),
    ])
}

/// Series `series`' samples in `(start, end]`: every 100 ms (offset by
/// `series` ms), a counter growing by 0–3.33 per sample, reset to 0 at 20 min
/// (series 3) and 45 min (series 7). Series 10 ends at 25 min with a stale
/// marker; series 11 starts at 35.5 min.
fn samples(series: usize, start: i64, end: i64) -> Vec<(i64, f64)> {
    let mut samples = Vec::new();
    let mut counter = 0.0;
    let mut ts = series as i64 + 1;
    while ts <= end {
        let increment = ((ts / SAMPLE_INTERVAL_MS) as u64)
            .wrapping_mul(2_654_435_761)
            .wrapping_add(series as u64 * 40_503)
            % 10;
        // Not integers: their sums round, so the order of a merge shows.
        counter += increment as f64 * 0.37;
        let reset_at = match series {
            3 => Some(20 * MINUTE_MS),
            7 => Some(45 * MINUTE_MS),
            _ => None,
        };
        if reset_at.is_some_and(|at| ts > at && ts - SAMPLE_INTERVAL_MS <= at) {
            counter = 0.0;
        }
        let value = match series {
            10 if ts > ENDS_MS + SAMPLE_INTERVAL_MS => break,
            10 if ts > ENDS_MS => Some(STALE_MARKER),
            11 if ts < STARTS_MS => None,
            _ => Some(counter),
        };
        if let Some(value) = value.filter(|_| ts > start) {
            samples.push((ts, value));
        }
        ts += SAMPLE_INTERVAL_MS;
    }
    samples
}

/// A view's appended rows: each run's `(start, end]` and batches.
type ViewRows = Vec<((i64, i64), Vec<Batch>)>;

/// The raw samples, one batch per minute, and the windows ingestion runs
/// store, as a deployment's store holds them.
struct MinuteStore {
    schema: SchemaRef,
    /// Each minute `(start, end]`'s samples, oldest first.
    raw: Vec<((i64, i64), Batch)>,
    /// Each view's appended rows.
    views: Mutex<BTreeMap<u64, ViewRows>>,
}

impl MinuteStore {
    fn new(dag: &PhysicalASAPDAG) -> Self {
        let schema = dag
            .nodes
            .iter()
            .find_map(|node| match &node.payload {
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { schema, .. }) => {
                    Some(Arc::new(schema.clone()))
                }
                _ => None,
            })
            .expect("one Scan");
        let end = *EVALUATIONS_MS.last().unwrap();
        let raw = (0..end / MINUTE_MS)
            .map(|minute| {
                let range = (minute * MINUTE_MS, (minute + 1) * MINUTE_MS);
                let rows = (0..SERIES)
                    .flat_map(|series| {
                        let labels = labels(series);
                        samples(series, range.0, range.1)
                            .into_iter()
                            .map(|(ts, value)| row(&schema, &labels, ts, value))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                (range, Batch::try_new(schema.clone(), rows).unwrap())
            })
            .collect();
        Self {
            schema,
            raw,
            views: Mutex::default(),
        }
    }

    fn append(&self, view: &MaterializedView, range: (i64, i64), batches: Vec<Batch>) {
        self.views
            .lock()
            .unwrap()
            .entry(view.node_id as u64)
            .or_default()
            .push((range, batches));
    }
}

/// One raw row of `schema`: the series identity, the timestamp, the value,
/// and each label column.
fn row(schema: &SchemaRef, labels: &BTreeMap<String, String>, ts: i64, value: f64) -> Vec<Value> {
    schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| match field.name.as_str() {
            PROMQL_SERIES_IDENTITY => Value::Utf8(encode_series_identity(labels).unwrap().into()),
            _ if Some(index) == schema.time_index => Value::Timestamp(ts),
            _ if field.dtype == FieldDataType::Plain(DataType::Float64) => Value::Float64(value),
            name => Value::Utf8(labels.get(name).cloned().unwrap_or_default().into()),
        })
        .collect()
}

fn stream_of<'a>(batches: Vec<Batch>) -> OutputStream<'a, Batch> {
    stream::iter(batches.into_iter().map(Ok)).boxed_local()
}

impl DataStore for MinuteStore {
    fn read_raw_data(
        &self,
        _: &Source,
        request: &RawDataRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        assert_eq!(request.schema, &self.schema);
        let (start, end) = request.time_range_ms.unwrap_or((i64::MIN, i64::MAX));
        Ok(stream_of(
            self.raw
                .iter()
                .filter(|((from, to), _)| *from < end && *to > start)
                .map(|(_, batch)| batch.clone())
                .collect(),
        ))
    }

    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        request: &MaterializedViewRequest,
        _: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error> {
        let (start, end) = request.time_range_ms;
        let views = self.views.lock().unwrap();
        let rows = views
            .get(&(view.node_id as u64))
            .map_or(&[][..], Vec::as_slice);
        let selected = rows
            .iter()
            .filter(|((from, to), _)| *from >= start && *to <= end)
            .collect::<Vec<_>>();
        let tiled = selected
            .first()
            .is_some_and(|((from, _), _)| *from == start)
            && selected.last().is_some_and(|((_, to), _)| *to == end)
            && selected.windows(2).all(|pair| pair[0].0 .1 == pair[1].0 .0);
        if !tiled {
            return Err(Error::Invalid(format!(
                "view {} has no windows tiling ({start}, {end}]",
                view.node_id
            )));
        }
        Ok(stream_of(
            selected
                .into_iter()
                .flat_map(|(_, batches)| batches.iter().cloned())
                .collect(),
        ))
    }
}

/// Every root's batches of one run of `dag`.
fn run(
    dag: &asap_executor::plan::PhysicalDAG<'static, Batch, SchemaRef>,
    roots: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    let ingestion = matches!(scope, Scope::Ingestion { .. });
    let context = RunContext::new(scope, Limits::default()).unwrap();
    block_on(async {
        if ingestion {
            dag.execute_ingestion(roots, context)
                .unwrap()
                .await
                .unwrap()
                .into_iter()
                .map(|batches| batches.iter().map(|batch| (**batch).clone()).collect())
                .collect()
        } else {
            let mut outputs = Vec::new();
            for mut output in dag.execute(roots, context).unwrap() {
                let mut batches = Vec::new();
                while let Some(batch) = output.next().await {
                    batches.push((*batch.unwrap()).clone());
                }
                outputs.push(batches);
            }
            outputs
        }
    })
}

/// Each evaluation's answer per job, as the bits of each `f64`.
type Answers = BTreeMap<i64, BTreeMap<String, String>>;

/// Ingest the samples into the plan's views, minute by minute, and
/// evaluate the query at each of [`EVALUATIONS_MS`].
fn answers(dag: &PhysicalASAPDAG) -> Answers {
    let store = Arc::new(MinuteStore::new(dag));
    let views = dag.materialized_views().unwrap();
    let ingestion = dag
        .ingestion_refresh_groups_of(&views)
        .into_iter()
        .map(|group| {
            let width = group.refresh_window.expect("a refresh window").width_ms as i64;
            let ids = group
                .views
                .iter()
                .map(|view| view.node_id as u64)
                .collect::<Vec<_>>();
            let compiled = compile(dag, &[], &ids).unwrap();
            (
                width,
                group.views,
                ids,
                compiled.instantiate(store.clone()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let roots = dag
        .roots
        .iter()
        .map(|&root| root as u64)
        .collect::<Vec<_>>();
    let query = compile(dag, &views, &roots)
        .unwrap()
        .instantiate(store.clone())
        .unwrap();
    let mut answers = Answers::new();
    let mut now = MINUTE_MS;
    while now <= *EVALUATIONS_MS.last().unwrap() {
        for (width, group_views, ids, dag) in &ingestion {
            if now % width == 0 {
                let scope = Scope::Ingestion {
                    window_start_ms: now - width,
                    window_end_ms: now,
                };
                for (view, batches) in group_views.iter().zip(run(dag, ids, scope)) {
                    store.append(view, (now - width, now), batches);
                }
            }
        }
        if EVALUATIONS_MS.contains(&now) {
            let outputs = run(
                &query,
                &roots,
                Scope::Query {
                    evaluation_time_ms: now,
                },
            );
            let by_job = outputs[0]
                .iter()
                .flat_map(Batch::to_rows)
                .map(|row| {
                    let job = row.iter().find_map(|value| match value {
                        Value::Utf8(text) if text.starts_with('j') => Some(text.to_string()),
                        _ => None,
                    });
                    let value = row.iter().find_map(|value| match value {
                        Value::Float64(value) => Some(*value),
                        _ => None,
                    });
                    (
                        job.expect("a job label"),
                        format!("{:#018x}", value.expect("a value").to_bits()),
                    )
                })
                .collect();
            answers.insert(now, by_job);
        }
        now += MINUTE_MS;
    }
    answers
}

fn recorded_answers_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/stored_window_merge_fixtures/sum_rate_1h_10hz_every_60s.json")
}

// The selected plan's answers at three evaluations, after counter resets,
// a series' end and another's start within the hour, are the hash merge's
// answers, bit for bit.
#[test]
fn sum_of_rate_over_stored_windows_answers_as_the_hash_merge_did() {
    let dag = selected_plan();
    assert_merges_stored_counter_windows(&dag);
    let answers = answers(&dag);
    assert_eq!(answers.len(), EVALUATIONS_MS.len());
    assert!(
        answers.values().all(|by_job| by_job.len() == 4),
        "{answers:?}"
    );
    let path = recorded_answers_path();
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&answers).unwrap() + "\n",
        )
        .unwrap();
    }
    let recorded: Answers = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(answers, recorded);
}
