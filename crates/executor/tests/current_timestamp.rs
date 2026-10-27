//! SQL `now()` (`CurrentTimestamp`) is the query's evaluation time: a scan
//! filtered on `ts >= now() - INTERVAL '5 minutes'` keeps the rows of the
//! five minutes up to the evaluation. Without an evaluation time (an
//! ingestion run) `now()` has no value and the run fails.
use asap_executor::{
    physical_planner::{bind_with_data_sources, compile},
    runtime::{Limits, RunContext, Scope},
    sources::{DataSources, MemorySource},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::Source;
use planner_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPDAGNode};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ExprSemantics, ScalarValue};
use planner_types::ir::schema::{DataType, Field, Schema};
use planner_types::ir::{NonASAPOp, Predicate, ScalarExpr};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const MINUTE_MS: i64 = 60_000;
const EVALUATION_MS: i64 = 1_800_000_000_000;

fn schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("user_id", DataType::Int64, false),
        ],
        0,
        vec![],
    )
}

/// `ts >= now() - INTERVAL '5 minutes'`.
fn last_five_minutes() -> Predicate<usize> {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(0)),
        op: CompareOpKind::Ge,
        right: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(ScalarExpr::CurrentTimestamp),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Interval {
                months: 0,
                days: 0,
                nanos: 5 * MINUTE_MS * 1_000_000,
            })),
            semantics: ExprSemantics::Sql,
        }),
        semantics: ExprSemantics::Sql,
    })
}

/// One `Scan` of table `http` with `predicates`.
fn scan_plan(predicates: Vec<Predicate<usize>>) -> PhysicalASAPDAG {
    PhysicalASAPDAG {
        nodes: vec![PhysicalASAPDAGNode {
            id: 0,
            payload: planner_types::ir::physical_export::PhysicalASAPOperatorPayload::NonASAP(
                NonASAPOp::Scan {
                    source: Source::Table {
                        table_ref: "http".into(),
                    },
                    predicates,
                    schema: schema(),
                },
            ),
            output_state: ExecutionDataState::QUERY_ROWS,
            materialization: Default::default(),
            output_schema: schema(),
            guarantee: None,
        }],
        edges: vec![],
        roots: vec![0],
        scalar_roots: vec![],
    }
}

/// One row a minute over the 10 minutes up to the evaluation, user = its
/// minute.
fn sources() -> DataSources {
    let schema = Arc::new(schema());
    let rows = (0..=10)
        .map(|minute| {
            vec![
                Value::Timestamp(EVALUATION_MS - 10 * MINUTE_MS + minute * MINUTE_MS),
                Value::Int64(minute),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            Source::Table {
                table_ref: "http".into(),
            },
            Arc::new(MemorySource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();
    sources
}

/// The users the scan keeps in a run of `scope`.
fn kept_users(scope: Scope) -> Result<Vec<i64>, String> {
    let dag = scan_plan(vec![last_five_minutes()]);
    let sources = sources();
    let bound = bind_with_data_sources(&dag, BTreeMap::new(), &[0], &sources).unwrap();
    let context = RunContext::new(scope, Limits::default()).unwrap();
    block_on(async {
        let mut output = bound
            .execute(&[0], context)
            .map_err(|e| e.to_string())?
            .remove(0);
        let mut users = Vec::new();
        while let Some(batch) = output.next().await {
            for row in batch.map_err(|e| e.to_string())?.to_rows() {
                let Value::Int64(user) = row[1] else {
                    panic!("user {:?}", row[1])
                };
                users.push(user);
            }
        }
        Ok(users)
    })
}

#[test]
fn now_is_the_query_evaluation_time() {
    let users = kept_users(Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    })
    .unwrap();
    // Minutes 5..=10: from five minutes before the evaluation on.
    assert_eq!(users, vec![5, 6, 7, 8, 9, 10]);
}

#[test]
fn now_has_no_value_in_an_ingestion_run() {
    let error = kept_users(Scope::Ingestion {
        window_start_ms: EVALUATION_MS - MINUTE_MS,
        window_end_ms: EVALUATION_MS,
        revision: 1,
    })
    .unwrap_err();
    assert!(error.contains("now()"), "{error}");
}

/// A compile check cuts a plan at its table scans: the plan compiles
/// without a reader, and a scan predicate the executor cannot evaluate
/// fails there, not at run time.
#[test]
fn compiling_a_scan_slot_checks_the_scan_predicates() {
    let dag = scan_plan(vec![last_five_minutes()]);
    compile(&dag, &BTreeSet::from([0]), &[0]).expect("now() - INTERVAL compiles");

    let not_boolean = scan_plan(vec![Predicate(ScalarExpr::Column(1))]);
    assert!(compile(&not_boolean, &BTreeSet::from([0]), &[0]).is_err());
}
