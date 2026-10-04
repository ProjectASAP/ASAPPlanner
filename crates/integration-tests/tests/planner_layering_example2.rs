//! #509 Example 2 status: one UnivMon over `src_ip` for distinct, entropy and
//! L2. Records what lowers, plans through `plan_stages` and executes exactly.
mod physical_common;
use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_optimizer::pass1::replacement::Realization;
use asap_plan_selection::{plan_stages, PlanningModels};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, Schema, SketchAlgorithm};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, Evidence, EvidenceSource, Predictability, QueryRecurrence, Rate,
    RootDemand,
};
use std::rc::Rc;

const WINDOW: &str = "ts >= now() - INTERVAL '1 minute'";
const Q1: &str = "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'";
const Q2: &str = "SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";
/// The design's Q3: its Int64 `c * c` can overflow, so it is not recognized.
const Q3: &str = "SELECT SQRT(SUM(c * c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";
/// Q3 with a floating product, which the L2 rule accepts.
const Q3_FLOAT: &str = "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("src_ip", DataType::Utf8, false),
        ]),
    )
}
fn target(epsilon: f64) -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon,
        delta: 0.01,
    }
}
fn intent(node: &OperatorNode, matches: fn(&AggIntent) -> bool) -> bool {
    OperatorNode::reachable(&Rc::new(node.clone())).iter().any(|n| {
        matches!(n.non_asap(), Some(NonASAPOp::Aggregate { measures, .. }) if measures.iter().any(matches))
    })
}
async fn workload() -> Vec<(Rc<OperatorNode>, AccuracyTarget)> {
    let mut roots = vec![];
    for (sql, epsilon) in [(Q1, 0.02), (Q2, 0.05), (Q3_FLOAT, 0.01)] {
        let root = lower_sql(sql, &catalog(), target(epsilon)).await.unwrap();
        roots.push((root, target(epsilon)));
    }
    roots
}

// The SQL frontend names all three Example 2 computations; the design's
// integer Q3 stays relational because its overflow is observable.
#[tokio::test]
async fn example2_queries_lower_to_frequency_intents() {
    let roots = workload().await;
    assert!(intent(&roots[0].0, |m| matches!(
        m,
        AggIntent::Cardinality { .. }
    )));
    assert!(intent(&roots[1].0, |m| matches!(
        m,
        AggIntent::FrequencyEntropy { .. }
    )));
    assert!(intent(&roots[2].0, |m| matches!(
        m,
        AggIntent::FrequencyL2 { .. }
    )));
    let design_q3 = lower_sql(Q3, &catalog(), target(0.01)).await.unwrap();
    assert!(!intent(&design_q3, |m| matches!(
        m,
        AggIntent::FrequencyL2 { .. }
    )));
}

// `WHERE ts >= now() - INTERVAL '1 minute'` becomes a predicate on the scan,
// not a per-measure FILTER or a TimeRange; Pass 1 accepts the aggregates.
#[tokio::test]
async fn example2_time_filter_is_a_scan_predicate() {
    for (root, _) in workload().await {
        let nodes = OperatorNode::reachable(&root);
        let scans: Vec<_> = nodes
            .iter()
            .filter_map(|n| match n.non_asap() {
                Some(NonASAPOp::Scan { predicates, .. }) => Some(predicates.clone()),
                _ => None,
            })
            .collect();
        assert!(scans.iter().all(|p| p.len() == 1), "{WINDOW} on every scan");
        assert!(scans
            .iter()
            .all(|p| matches!(&p[0].0, ScalarExpr::Compare { .. })));
        assert!(nodes.iter().all(|n| !matches!(
            n.non_asap(),
            Some(NonASAPOp::TimeRange { .. } | NonASAPOp::Filter { .. })
        )));
        assert!(nodes.iter().all(|n| !matches!(
            n.non_asap(),
            Some(NonASAPOp::Aggregate { filters, .. }) if filters.iter().any(Option::is_some)
        )));
    }
}

// `plan_stages` plans the workload, and Pass 1 offers UnivMon to each of the
// three statistics. Sharing one UnivMon needs the summary-capability rule.
#[tokio::test]
async fn example2_plans_through_the_stage_pipeline() {
    let roots = workload().await;
    let demand: Vec<_> = roots
        .iter()
        .map(|(_, t)| RootDemand {
            accuracy: Some(t.clone()),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            predictability: Predictability::default(),
            latency_ms: None,
        })
        .collect();
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        data_ingestion_interval: Evidence::default(),
        ingestion_volume: Evidence::default(),
        ingestion_rate: declared(Rate(100_000.0)),
        input_cardinality: declared(10_000_000),
        distribution: Evidence::default(),
        metric_types: Default::default(),
    };
    let run = plan_stages(
        roots
            .into_iter()
            .enumerate()
            .map(|(i, (root, _))| (i, QueryRoot::Operator(root)))
            .collect(),
        &demand,
        &data,
        PlanningModels::builtin(),
        0,
    )
    .expect("Example 2 plans");
    let inventory = &run.stage1[0].inventory;
    for statistic in [
        (|m: &AggIntent| matches!(m, AggIntent::Cardinality { .. })) as fn(&AggIntent) -> bool,
        |m| matches!(m, AggIntent::FrequencyEntropy { .. }),
        |m| matches!(m, AggIntent::FrequencyL2 { .. }),
    ] {
        let target = inventory
            .targets
            .iter()
            .find(|t| matches!(t.target.non_asap(), Some(NonASAPOp::Aggregate { measures, .. }) if measures.iter().any(statistic)))
            .expect("statistic target");
        assert!(target
            .alternatives
            .iter()
            .any(|a| matches!(a, Realization::PassThrough)));
        assert!(target.alternatives.iter().any(
            |a| matches!(a, Realization::Sketch(kind) if kind.algorithm() == &SketchAlgorithm::UnivMon)
        ));
    }
}

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

// The exact candidate of each query executes natively without the time
// filter. With it, binding fails: the runtime has no `now()` yet.
#[tokio::test]
async fn example2_exact_candidates_execute() {
    let rows: Vec<_> = ["a", "a", "b"]
        .into_iter()
        .map(|ip| vec![Value::Timestamp(0), Value::Utf8(ip.into())])
        .collect();
    let mut results = vec![];
    for (sql, epsilon) in [(Q1, 0.02), (Q2, 0.05), (Q3_FLOAT, 0.01)] {
        let windowed = lower_sql(sql, &catalog(), target(epsilon)).await.unwrap();
        let message = bind_error(&windowed);
        assert!(message.contains("CurrentTimestamp"), "{message}");
        let unwindowed = sql.replace(&format!(" WHERE {WINDOW}"), "");
        let root = lower_sql(&unwindowed, &catalog(), target(epsilon))
            .await
            .unwrap();
        results.push(physical_common::execute_raw_rows(&root, rows.clone()));
    }
    assert!(matches!(results[0][..], [ref r] if matches!(r[..], [Value::Int64(2)])));
    let entropy = -(2.0_f64 / 3.0) * (2.0_f64 / 3.0).ln() - (1.0_f64 / 3.0) * (1.0_f64 / 3.0).ln();
    assert!(matches!(results[1][0][0], Value::Float64(v) if (v - entropy).abs() < 1e-12));
    assert!(matches!(results[2][0][0], Value::Float64(v) if (v - 5.0_f64.sqrt()).abs() < 1e-12));
}

/// The error binding `root` against an empty `flows` connector reports.
fn bind_error(root: &Rc<OperatorNode>) -> String {
    use asap_executor::physical_planner::bind_with_data_sources;
    use asap_executor::sources::{DataSources, MemorySource};
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPOperatorPayload};
    use std::{collections::BTreeMap, sync::Arc};
    let wire = physical_common::compile_physical_asap_dag(root).unwrap();
    let (source, schema) = wire
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::Relational {
                operator: NonASAPOpKind::Scan { source, .. },
            } => Some((source.clone(), node.output_schema.clone())),
            _ => None,
        })
        .unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source,
            Arc::new(MemorySource::new(Arc::new(schema), vec![]).unwrap()),
        )
        .unwrap();
    bind_with_data_sources(
        &wire,
        BTreeMap::new(),
        &[u64::from(wire.roots[0].0)],
        &sources,
    )
    .err()
    .expect("binding fails")
    .to_string()
}
