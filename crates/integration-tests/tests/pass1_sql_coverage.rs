//! Pass 1 alternatives over SQL row sources compose, compile and execute.
mod physical_common;
use std::collections::BTreeMap;
use std::rc::Rc;

use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_optimizer::pass1::logical_candidates::{
    compose_logical_candidate, enumerate_choices, enumerate_local_logical_candidates,
    LocalLogicalCandidates,
};
use asap_plan_selection::{plan_stages, PlanningModels};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, Evidence, EvidenceSource, Predictability, QueryRecurrence, Rate,
    RootDemand,
};

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("src_ip", DataType::Utf8, false),
        ]),
    )
}

fn target() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    }
}

fn rows() -> Vec<Vec<Value>> {
    ["a", "a", "b", "a", "c", "b"]
        .into_iter()
        .map(|ip| vec![Value::Timestamp(0), Value::Utf8(ip.into())])
        .collect()
}

async fn inventory(sql: &str) -> LocalLogicalCandidates<usize> {
    let root = lower_sql(sql, &catalog(), target()).await.unwrap();
    enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root))], &BTreeMap::new())
        .unwrap()
}

fn has_estimate(root: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(root)
        .iter()
        .any(|n| matches!(n.operator, Operator::ASAP(ASAPOp::SummaryEstimate { .. })))
}

/// `COUNT(*) … GROUP BY src_ip` (#509 Example 2's inner query) reads no
/// sample value. Every candidate composes and compiles; the exact ones,
/// the exact `Count` accumulator included, count rows per `src_ip`.
#[tokio::test]
async fn sql_count_star_group_by_candidates_compose_and_execute() {
    let inventory = inventory("SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip").await;
    let choices = enumerate_choices(&inventory, usize::MAX);
    assert!(choices.len() > 2, "exact and summary candidates");
    let mut executed = 0;
    for choice in choices {
        let roots = compose_logical_candidate(&inventory, &choice)
            .unwrap_or_else(|e| panic!("{choice:?} composes: {e}"));
        let QueryRoot::Operator(root) = &roots[0].1 else {
            panic!("operator root")
        };
        physical_common::compile_physical_asap_dag(root)
            .unwrap_or_else(|e| panic!("{choice:?} compiles: {e}"));
        if has_estimate(root) {
            continue;
        }
        let mut rows = physical_common::execute_raw_rows(root, rows());
        rows.sort_by_key(|row| format!("{row:?}"));
        let counts: Vec<_> = rows
            .iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Utf8(ip), Value::Int64(n)) => (ip.to_string(), *n),
                other => panic!("{choice:?}: unexpected row {other:?}"),
            })
            .collect();
        assert_eq!(
            counts,
            [("a".into(), 3), ("b".into(), 2), ("c".into(), 1)],
            "{choice:?}"
        );
        executed += 1;
    }
    assert_eq!(executed, 2, "pass-through and the exact Count accumulator");
}

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

/// #509 Example 2's design queries (its integer Q3 keeps `COUNT(*) GROUP BY
/// src_ip` as a target): every candidate builds through Stages 1 and 2.
/// Stage 3 may still reject one, e.g. for accuracy.
#[tokio::test]
async fn example2_design_candidates_all_build() {
    const QUERIES: [&str; 3] = [
        "SELECT COUNT(DISTINCT src_ip) FROM flows",
        "SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip)",
        "SELECT SQRT(SUM(c * c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip)",
    ];
    let mut roots = vec![];
    for (i, sql) in QUERIES.into_iter().enumerate() {
        let root = lower_sql(sql, &catalog(), target()).await.unwrap();
        roots.push((i, QueryRoot::Operator(root)));
    }
    let demand = vec![
        RootDemand {
            accuracy: Some(target()),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            predictability: Predictability::default(),
        };
        roots.len()
    ];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: declared(Rate(100_000.0)),
        input_cardinality: declared(10_000_000),
        ..Default::default()
    };
    let run = plan_stages(roots, &demand, &data, PlanningModels::builtin(), 4096).unwrap();
    let enumeration = run.enumeration.unwrap();
    let unbuilt: Vec<_> = enumeration
        .selection
        .rejected
        .iter()
        .filter(|r| r.reason.starts_with("Stage "))
        .collect();
    assert!(enumeration.candidates.len() > 1);
    assert!(unbuilt.is_empty(), "{unbuilt:#?}");
}
