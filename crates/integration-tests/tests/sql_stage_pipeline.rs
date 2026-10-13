//! SQL scenarios the removed legacy search covered, through the planner's
//! stage pipeline (`run_stage1_to_stage3`): ClickHouse temporal rate and
//! increase, and a join whose two sides are each summary-bound aggregates.

use std::rc::Rc;

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_integration_tests::planning::selected_dag;
use asap_types::ir::operator_properties::Reduction;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{ExactKind, ExactParams, FieldDataType};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

/// `metrics(ts, service, latency, bytes)`, keyed by `(ts, service)`.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("latency", DataType::Float64),
                col("bytes", DataType::Int64),
            ],
            0,
            vec![vec![0, 1]],
        ),
    )
}

/// Lower ClickHouse SQL and take the plan the stage pipeline selects.
async fn plan(sql: &str) -> Rc<OperatorNode> {
    let pre = lower_sql_dialect(
        sql,
        &catalog(),
        SqlDialect::ClickhouseSQL,
        AccuracyTarget::Exact,
    )
    .await
    .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    selected_dag(pre, AccuracyTarget::Exact)
}

/// Every reachable `SummaryAgg` node's family and reduction.
fn summary_aggs(root: &Rc<OperatorNode>) -> Vec<(FieldDataType, Reduction)> {
    OperatorNode::reachable(root)
        .into_iter()
        .filter_map(|node| match &node.operator {
            Operator::ASAP(ASAPOp::SummaryAgg {
                family, reduction, ..
            }) => Some((family.clone(), reduction.clone())),
            _ => None,
        })
        .collect()
}

/// ClickHouse `asap_rate` / `asap_increase` select a per-entity exact
/// rate / increase accumulator.
#[tokio::test]
async fn clickhouse_temporal_rate_and_increase_select_exact_accumulators() {
    for (function, expected) in [
        (
            "asap_rate",
            FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        ),
        (
            "asap_increase",
            FieldDataType::ExactAggregate(ExactKind::Increase, ExactParams::Increase),
        ),
    ] {
        let root = plan(&format!(
            "SELECT service, {function}(latency, ts, 300000) AS v \
             FROM metrics WHERE bytes > 0 GROUP BY service"
        ))
        .await;
        assert_eq!(
            summary_aggs(&root),
            vec![(expected, Reduction::PerEntity)],
            "{function}"
        );
    }
}

/// A join stays a query-time node while both derived-table rate aggregates
/// below it are bound to summaries.
#[tokio::test]
async fn sql_join_binds_both_temporal_aggregate_sides() {
    let root = plan(
        "SELECT a.service, a.v / b.v AS ratio FROM \
         (SELECT service, asap_rate(latency, ts, 300000) AS v FROM metrics WHERE service='errors' GROUP BY service) a \
         INNER JOIN \
         (SELECT service, asap_rate(latency, ts, 300000) AS v FROM metrics WHERE service='requests' GROUP BY service) b \
         ON b.service=a.service",
    )
    .await;
    let joins: Vec<_> = OperatorNode::reachable(&root)
        .into_iter()
        .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Join { .. })))
        .collect();
    assert_eq!(joins.len(), 1, "one query-time join");
    let sides = joins[0].children();
    assert_eq!(sides.len(), 2);
    let rate = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    for side in sides {
        assert!(
            summary_aggs(side)
                .iter()
                .any(|(family, reduction)| family == &rate && *reduction == Reduction::PerEntity),
            "join side has no rate summary: {side:?}"
        );
    }
}
