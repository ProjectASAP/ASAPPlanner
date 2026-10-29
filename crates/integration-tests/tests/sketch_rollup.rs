//! A regrouping `SummaryMerge` (rollup) answers a coarse grouping from the
//! states of a finer one: `GROUP BY job` from states built `GROUP BY region,
//! job`, merging each job's per-region states. For every mergeable sketch
//! the planner offers a SQL grouped query, the rolled-up answer equals the
//! direct build's (deterministic merges) or is within the sketch's bound
//! (KLL), and its coverage is the direct build's.
mod physical_common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_executor::{
    physical_planner::compile,
    runtime::{Limits, RunContext, Scope},
    sources::InMemoryDataStore,
    values::{Batch, Value},
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::planning::explicit_local_candidates;
use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::summary_coverage::CoverageError;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::{
    DataType, ExactKind, ExactParams, Field, FieldDataType, Schema, SketchAlgorithm,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use futures::StreamExt;

const REGIONS: [&str; 3] = ["us", "eu", "ap"];
const JOBS: [&str; 4] = ["api", "web", "db", "cache"];

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "http",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("job", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
            Field::plain("user_id", DataType::Float64, false),
        ]),
    )
}

/// 6,000 rows over every (region, job): a heavy-tailed latency and a user
/// id from 800, from a fixed linear congruential generator.
fn rows() -> Vec<(&'static str, &'static str, f64, f64)> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as f64 / (1u64 << 31) as f64
    };
    (0..6_000)
        .map(|_| {
            let region = REGIONS[(next() * 3.0) as usize % 3];
            let job = JOBS[(next() * 4.0) as usize % 4];
            let latency = 10.0 / (1.0 - next()).max(1e-6).sqrt();
            let user = (next() * 800.0).floor();
            (region, job, latency, user)
        })
        .collect()
}

/// `node` with every Int64 column typed Float64. The Planner types a SQL
/// `COUNT(DISTINCT)` estimate Int64 and the executor returns Float64, a
/// known SQL execution gap this test does not cover.
fn as_float64(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    let rebuilt = node.with_new_children(as_float64).unwrap();
    let mut schema = node.schema.clone();
    for field in &mut schema.fields {
        if field.dtype == FieldDataType::Plain(DataType::Int64) {
            field.dtype = FieldDataType::Plain(DataType::Float64);
        }
    }
    Rc::new(
        OperatorNode::with_schema(rebuilt.operator, schema).with_guarantee(node.guarantee.clone()),
    )
}

/// The answers of the logical DAG `root` over [`rows`], by group key.
fn execute(root: &Rc<OperatorNode>) -> BTreeMap<String, f64> {
    let root = &as_float64(root);
    let dag = physical_common::compile_physical_asap_dag(root).unwrap();
    let scan = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            )
        })
        .expect("one scan");
    let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) = &scan.payload else {
        unreachable!()
    };
    let schema = Arc::new(scan.output_schema.clone());
    let plan = compile(&dag, &[], &[dag.roots[0] as u64]).unwrap();
    let table: Vec<Vec<Value>> = rows()
        .into_iter()
        .map(|(region, job, latency, user)| {
            vec![
                Value::Timestamp(0),
                Value::Utf8(region.into()),
                Value::Utf8(job.into()),
                Value::Float64(latency),
                Value::Float64(user),
            ]
        })
        .collect();
    let store = InMemoryDataStore::default()
        .with_raw_data(
            source.clone(),
            schema.clone(),
            vec![Batch::try_new(schema.clone(), table).unwrap()],
        )
        .unwrap();
    let bound = plan.instantiate(Arc::new(store)).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        // The UnivMon states planned here are large: the merge's copy of
        // twelve of them exceeds the default limit.
        Limits {
            max_bytes: 1 << 32,
            ..Limits::default()
        },
    )
    .unwrap();
    futures::executor::block_on(async {
        let mut stream = bound.execute(plan.roots(), context).unwrap().remove(0);
        let mut answers = BTreeMap::new();
        while let Some(batch) = stream.next().await {
            for row in batch.unwrap().to_rows() {
                let (keys, value) = row.split_at(row.len() - 1);
                let key = keys
                    .iter()
                    .map(|value| match value {
                        Value::Utf8(text) => text.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let value = match value[0] {
                    Value::Float64(value) => value,
                    Value::Int64(value) => value as f64,
                    ref other => panic!("not a number: {other:?}"),
                };
                assert!(answers.insert(key, value).is_none(), "one row per group");
            }
        }
        answers
    })
}

/// The only `SummaryAgg` of `root`.
fn build(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let builds: Vec<_> = OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .collect();
    match builds.as_slice() {
        [build] => Some(Rc::clone(build)),
        _ => None,
    }
}

/// `node` with `from` replaced by `to`.
fn replace(
    node: &Rc<OperatorNode>,
    from: &Rc<OperatorNode>,
    to: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    if Rc::ptr_eq(node, from) {
        return Rc::clone(to);
    }
    Rc::new(
        node.with_new_children(|child| replace(child, from, to))
            .unwrap()
            .with_guarantee(node.guarantee.clone()),
    )
}

/// `coarse`, a build by `job`, as the same build by `(region, job)`
/// regrouped by `job`: each output state merges its job's per-region
/// states.
fn rolled_up(coarse: &OperatorNode) -> Rc<OperatorNode> {
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction: Reduction::Reduce(by),
        grouping,
        filter,
    }) = coarse.asap()
    else {
        panic!("a grouped build: {:?}", coarse.operator)
    };
    let region = child.schema.column_id("region").unwrap();
    assert_eq!(by.keys().len(), 1);
    let finer = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: Rc::clone(child),
        family: family.clone(),
        input: input.clone(),
        reduction: Reduction::by(vec![region, by.keys()[0]]),
        grouping: grouping.clone(),
        filter: filter.clone(),
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![finer],
        group_by: Some(vec![1]),
        newest_panes: None,
    }))
    .unwrap()
}

/// Every local sketch rule for `sql` (one build by `job`), each with its
/// rollup from `(region, job)`: the family, the direct root and the
/// rolled-up root. Explicit rule fixtures keep this execution test independent
/// of the search archive and its tighter, potentially much larger capacities.
async fn direct_and_rolled_up(
    sql: &str,
    accuracy: AccuracyTarget,
) -> Vec<(SketchAlgorithm, Rc<OperatorNode>, Rc<OperatorNode>)> {
    let root = lower_sql(sql, &catalog(), accuracy.clone()).await.unwrap();
    let mut out = Vec::new();
    for candidate in explicit_local_candidates(root) {
        let Some(build) = build(&candidate) else {
            continue;
        };
        let Some(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) = build.asap()
        else {
            continue;
        };
        let merge = rolled_up(&build);
        assert_eq!(
            merge.coverage().expect("a valid rollup").definition_id(),
            build.coverage().unwrap().definition_id(),
            "{kind:?}: the rolled-up states cover what the direct build does"
        );
        let rolled = replace(&candidate, &build, &merge);
        rolled.validate_structure().unwrap();
        out.push((*kind.algorithm(), candidate, rolled));
    }
    out
}

/// Distinct users per job: KMV, HLL and UnivMon states of `(region, job)`
/// merged per job equal each job's state built directly (KMV keeps the k
/// smallest hashes of the union, HLL each register's maximum, UnivMon's
/// counters add), so every answer is the direct build's.
#[tokio::test]
async fn distinct_counts_rolled_up_equal_the_direct_build() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative);
    let pairs = direct_and_rolled_up(
        "SELECT job, COUNT(DISTINCT user_id) AS value FROM http GROUP BY job",
        accuracy,
    )
    .await;
    let families: Vec<_> = pairs.iter().map(|(family, ..)| *family).collect();
    assert!(families.contains(&SketchAlgorithm::Kmv), "{families:?}");
    for (family, direct, rolled) in pairs {
        let (direct, rolled) = (execute(&direct), execute(&rolled));
        assert_eq!(direct.len(), JOBS.len());
        assert_eq!(rolled, direct, "{family:?}");
    }
}

/// p99 latency per job. DDSketch's bucket counts add, so the merged state
/// is the direct build's and so is its answer. KLL's compactions are
/// randomized, so its rolled-up answer differs, but stays within its rank
/// bound of the exact p99, as the direct build's does.
#[tokio::test]
async fn quantiles_rolled_up_keep_the_sketch_bound() {
    const EPSILON: f64 = 0.05;
    for metric in [AccuracyMetric::Rank, AccuracyMetric::Relative] {
        let accuracy = AccuracyTarget::max_error(EPSILON)
            .failure_probability(0.01)
            .accuracy_metric(metric);
        let pairs = direct_and_rolled_up(
            "SELECT job, approx_percentile_cont(latency, 0.99) AS value FROM http GROUP BY job",
            accuracy,
        )
        .await;
        for (family, direct, rolled) in pairs {
            let (direct, rolled) = (execute(&direct), execute(&rolled));
            assert_eq!(direct.len(), JOBS.len());
            match family {
                SketchAlgorithm::DDSketch => assert_eq!(rolled, direct),
                SketchAlgorithm::Kll => {
                    for (job, answer) in rolled {
                        let mut latencies: Vec<f64> = rows()
                            .into_iter()
                            .filter(|row| row.1 == job)
                            .map(|row| row.2)
                            .collect();
                        latencies.sort_by(f64::total_cmp);
                        let rank = latencies.partition_point(|&v| v <= answer) as f64
                            / latencies.len() as f64;
                        assert!(
                            (rank - 0.99).abs() <= EPSILON,
                            "{job}: rank {rank} of {answer}"
                        );
                    }
                }
                other => panic!("unexpected quantile family {other:?}"),
            }
        }
    }
}

/// A regrouping merge keys only by group keys of its input states (or by
/// none, a global aggregate), and never merges counter states of different
/// groups.
#[tokio::test]
async fn invalid_rollups_are_rejected() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative);
    let root = lower_sql(
        "SELECT job, COUNT(DISTINCT user_id) AS value FROM http GROUP BY job",
        &catalog(),
        accuracy.clone(),
    )
    .await
    .unwrap();
    let candidate = explicit_local_candidates(root)
        .into_iter()
        .find(|candidate| {
            build(candidate).is_some_and(|build| {
                matches!(
                    build.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(..),
                        ..
                    })
                )
            })
        })
        .unwrap();
    let merge = rolled_up(&build(&candidate).unwrap());
    let finer = Rc::clone(merge.children()[0]);
    let regroup = |keys: Vec<usize>| {
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![Rc::clone(&finer)],
            group_by: Some(keys),
            newest_panes: None,
        }))
        .map_err(|error| error.to_string())
    };
    // The state column is no key; nor is a column twice.
    for keys in [vec![2], vec![1, 1]] {
        assert!(regroup(keys.clone()).is_err(), "{keys:?}");
    }
    // No keys: the global build over the same rows.
    assert!(regroup(vec![]).unwrap().coverage().is_some());
    // Both keys, swapped, are still group keys of the input.
    assert!(regroup(vec![1, 0]).unwrap().coverage().is_some());
    // Rate states merge consecutive windows of one series, not groups.
    let Some(ASAPOp::SummaryAgg {
        child,
        input,
        reduction,
        grouping,
        ..
    }) = finer.asap()
    else {
        unreachable!()
    };
    let rates = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: Rc::clone(child),
        family: FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        input: input.clone(),
        reduction: reduction.clone(),
        grouping: grouping.clone(),
        filter: None,
    }))
    .unwrap();
    let merged_rates = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![rates],
        group_by: Some(vec![1]),
        newest_panes: None,
    }));
    assert!(merged_rates
        .unwrap_err()
        .to_string()
        .contains(&CoverageError::FamilyCannotRegroup.to_string()));
}
