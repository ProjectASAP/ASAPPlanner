//! Planner-selected PromQL computation compiles from the timed DAG alone;
//! the deployment supplies only raw rows at its time-series scans.

use asap_executor::sources::InMemoryDataStore;
use asap_executor::{
    physical_planner::{compile, promql_rows},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};

use futures::{executor::block_on, StreamExt};
use planner_types::accuracy::AccuracyTarget;
use planner_types::ir::operator::maintained_population::PopulationStatistic;
use planner_types::ir::operator::Source;
use planner_types::ir::schema::*;
use planner_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};

use planner_types::workload::*;
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

fn lower(query: &str) -> Rc<planner_types::ir::OperatorNode> {
    lower_with(query, AccuracyTarget::exact())
}

fn lower_with(query: &str, accuracy: AccuracyTarget) -> Rc<planner_types::ir::OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy,
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(60_000),
        )),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload)
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root")
}

/// The population sub-DAG of `query`, which computes `statistic` grouped by
/// `grouping` over one selector.
fn population_dag(
    query: &str,
    statistic: PopulationStatistic,
    grouping: &[&str],
) -> PhysicalASAPDAG {
    let root = promql_rows::with_series_identity(&lower(query)).unwrap();
    let selected = planner_types::test_support::forwarders::population_over_selector(
        &root, statistic, grouping,
    );
    planner_types::test_support::forwarders::physical_asap_dag_at_query_time(&selected).unwrap()
}

/// The time-series scans: each metric's raw data slot and its schema.
fn raw_inputs(dag: &PhysicalASAPDAG) -> Vec<(Source, Arc<Schema>, String)> {
    let mut inputs: Vec<(Source, Arc<Schema>, String)> = vec![];
    for node in &dag.nodes {
        if let PhysicalASAPOperatorPayload::NonASAP(planner_types::ir::NonASAPOp::Scan {
            source: source @ Source::TimeSeries { metric },
            ..
        }) = &node.payload
        {
            if !inputs.iter().any(|(bound, _, _)| bound == source) {
                inputs.push((
                    source.clone(),
                    Arc::new(node.output_schema.clone()),
                    metric.clone(),
                ));
            }
        }
    }
    inputs
}

type RawDataSample = (&'static str, &'static str, &'static str, i64, f64);

/// Compile, round-trip, bind raw `(metric, job, instance, ts, value)` samples,
/// and return the root's batches.
fn execute(
    dag: &PhysicalASAPDAG,
    samples: &[RawDataSample],
    end: i64,
) -> Result<Vec<asap_executor::runtime::SharedValue<Batch>>, String> {
    execute_relabeled(dag, samples, end, &BTreeMap::new())
}

/// [`execute`], supplying samples of each instance in `relabel` under its
/// `(__name__, instance)` instead.
fn execute_relabeled(
    dag: &PhysicalASAPDAG,
    samples: &[RawDataSample],
    end: i64,
    relabel: &BTreeMap<&str, (&str, &str)>,
) -> Result<Vec<asap_executor::runtime::SharedValue<Batch>>, String> {
    let program = compile(dag, &[], &[dag.roots[0] as u64]).map_err(|e| e.to_string())?;
    let mut store = InMemoryDataStore::default();
    for (source, schema, metric) in raw_inputs(dag) {
        let rows = samples
            .iter()
            .filter(|sample| sample.0 == metric)
            .map(|(name, job, instance, at, value)| {
                let (name, instance) = relabel.get(instance).copied().unwrap_or((*name, *instance));
                let labels = BTreeMap::from([
                    ("__name__".to_string(), name.to_string()),
                    ("job".into(), job.to_string()),
                    ("instance".into(), instance.to_string()),
                ]);
                if schema
                    .fields
                    .iter()
                    .any(|f| f.name == promql_rows::SERIES_IDENTITY_COLUMN)
                {
                    promql_rows::series_row(&schema, &labels, *at, *value).unwrap()
                } else {
                    schema
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| match f.name.as_str() {
                            _ if Some(i) == schema.time_index => Value::Timestamp(*at),
                            "value" => Value::Float64(*value),
                            label => Value::Utf8(labels[label].clone().into()),
                        })
                        .collect()
                }
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        store = store
            .with_raw_data(source, schema, vec![batch])
            .map_err(|e| e.to_string())?;
    }
    let physical_dag = program
        .instantiate(Arc::new(store))
        .map_err(|e| e.to_string())?;
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: end,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = physical_dag
            .execute(program.roots(), context)
            .map_err(|e| e.to_string())?
            .remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            batches.push(batch.map_err(|e| e.to_string())?);
        }
        Ok(batches)
    })
}

/// [`execute`], returning `(job, value)` rows of the root.
fn run(
    dag: &PhysicalASAPDAG,
    samples: &[RawDataSample],
    end: i64,
) -> Result<BTreeMap<String, f64>, String> {
    let mut rows = BTreeMap::new();
    for batch in execute(dag, samples, end)? {
        let job = batch.schema().fields.iter().position(|f| f.name == "job");
        for row in &batch.to_rows() {
            let key = match job.map(|i| &row[i]) {
                Some(Value::Utf8(job)) => job.to_string(),
                _ => String::new(),
            };
            let value = match row.last() {
                Some(Value::Float64(v)) => *v,
                Some(Value::Int64(v)) => *v as f64,
                other => return Err(format!("unexpected value {other:?}")),
            };
            assert!(rows.insert(key, value).is_none(), "duplicate output group");
        }
    }
    Ok(rows)
}

const SAMPLES: &[RawDataSample] = &[
    ("m", "api", "a", 10_000, 4.),
    ("m", "api", "a", 50_000, 1.),
    ("m", "api", "b", 40_000, 7.),
    ("m", "api", "c", 30_000, 2.),
    ("m", "db", "d", 20_000, 5.),
];

fn reference(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

// Current-series aggregates read the latest member values, matching the
// backend CurrentSeriesStore formulas (PromQL quantile interpolation).
#[test]
fn population_aggregates_match_current_series_reference() {
    // Latest values: api = {a: 1, b: 7, c: 2}; db = {d: 5}.
    for (query, statistic, expected) in [
        (
            "sum by (job) (m)",
            PopulationStatistic::Sum,
            reference(&[("api", 10.), ("db", 5.)]),
        ),
        (
            "count by (job) (m)",
            PopulationStatistic::Count,
            reference(&[("api", 3.), ("db", 1.)]),
        ),
        (
            "avg by (job) (m)",
            PopulationStatistic::Average,
            reference(&[("api", 10. / 3.), ("db", 5.)]),
        ),
        // Sorted api = [1, 2, 7]; rank 0.25 * 2 = 0.5 → 1.5.
        (
            "quantile by (job) (0.25, m)",
            PopulationStatistic::Quantile { q: 0.25 },
            reference(&[("api", 1.5), ("db", 5.)]),
        ),
    ] {
        let dag = population_dag(query, statistic, &["job"]);
        assert_eq!(run(&dag, SAMPLES, 60_000).unwrap(), expected, "{query}");
    }
}

// A global evaluation of an empty population is an empty vector, as in PromQL.
#[test]
fn global_population_aggregate_of_no_members_is_empty() {
    // Latest values are [1, 2, 5, 7] at 60s; every member has expired by 1000s.
    for (query, statistic, expected) in [
        ("sum(m)", PopulationStatistic::Sum, 15.),
        ("count(m)", PopulationStatistic::Count, 4.),
        (
            "quantile(0.5, m)",
            PopulationStatistic::Quantile { q: 0.5 },
            3.5,
        ),
    ] {
        let dag = population_dag(query, statistic, &[]);
        let live = run(&dag, SAMPLES, 60_000).unwrap();
        assert_eq!(live, reference(&[("", expected)]), "{query}");
        assert!(run(&dag, SAMPLES, 1_000_000).unwrap().is_empty(), "{query}");
    }
}

// Current-series sums and averages are compensated like Prometheus, and an
// overflowing running sum does not turn the average into +Inf.
#[test]
fn population_sums_and_averages_are_compensated() {
    let cancel: &[RawDataSample] = &[
        ("m", "api", "a", 50_000, 1e100),
        ("m", "api", "b", 50_000, 1.),
        ("m", "api", "c", 50_000, -1e100),
    ];
    let huge: &[RawDataSample] = &[
        ("m", "api", "a", 50_000, 1.7e308),
        ("m", "api", "b", 50_000, 1.7e308),
    ];
    for (query, statistic, samples, expected) in [
        ("sum by (job) (m)", PopulationStatistic::Sum, cancel, 1.),
        (
            "avg by (job) (m)",
            PopulationStatistic::Average,
            cancel,
            1. / 3.,
        ),
        (
            "avg by (job) (m)",
            PopulationStatistic::Average,
            huge,
            1.7e308,
        ),
    ] {
        let dag = population_dag(query, statistic, &["job"]);
        assert_eq!(
            run(&dag, samples, 60_000).unwrap(),
            reference(&[("api", expected)]),
            "{query}"
        );
    }
}
