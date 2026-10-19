//! Exact accumulators built by the columnar `SummaryAgg` are numeric
//! records; merging and evaluating them gives what the same states give as
//! kernel objects (states read back from rows).
use asap_executor::operators::{Operator, SummaryEvaluation};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{
    DataType, ExactKind, ExactParams, Field, FieldDataType, Schema,
};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::summary_kernels::exact::ExactEvaluation;
use asap_executor::values::{Batch, SchemaRef, Value};
use asap_executor::Statistic;
use futures::{executor::block_on, StreamExt};
use std::sync::Arc;

fn field(name: &str, dtype: DataType) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
        table: None,
    }
}

fn input() -> SchemaRef {
    Arc::new(Schema {
        fields: vec![
            field("series", DataType::Utf8),
            field("ts", DataType::Timestamp),
            field("value", DataType::Float64),
        ],
        time_index: Some(1),
        unique_keys: vec![],
        closed: false,
    })
}

/// Samples of 3 series in `(start, start + 1000]`, counters with a reset.
fn pane_input(start: i64) -> Batch {
    let rows = (1..=40)
        .map(|i| {
            let series = i % 3;
            let value = if i == 20 {
                1.0
            } else {
                (start / 10 + i * 3) as f64
            };
            vec![
                Value::Utf8(format!("s{series}").into()),
                Value::Timestamp(start + i * 25),
                Value::Float64(value),
            ]
        })
        .collect();
    Batch::try_new(input(), rows).unwrap()
}

fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 2_000,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap()
}

/// Run `operator` over in-memory `inputs` (one source each); its batches.
fn run(operator: Operator, inputs: Vec<Batch>) -> Vec<Batch> {
    let mut dag = PhysicalDAG::default();
    let schema = inputs[0].schema().clone();
    dag.add(0, vec![], Operator::source(schema, inputs).unwrap())
        .unwrap();
    dag.add(1, vec![0], operator).unwrap();
    block_on(async {
        let mut output = dag.execute(&[1], context()).unwrap().remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    })
}

fn statistic(kind: &ExactKind) -> Statistic {
    match kind {
        ExactKind::Sum => Statistic::Sum,
        ExactKind::Count => Statistic::Count,
        ExactKind::Min => Statistic::Min,
        ExactKind::Max => Statistic::Max,
        ExactKind::Rate => Statistic::Rate,
        ExactKind::Increase => Statistic::Increase,
        _ => unreachable!(),
    }
}

// For each exact family: two panes built as numeric records, merged and
// evaluated, equal the same panes read back as kernel objects (all objects,
// or one of each), row for row.
#[test]
fn exact_records_merge_and_evaluate_as_kernel_objects_do() {
    for (kind, params) in [
        (ExactKind::Sum, ExactParams::Sum),
        (ExactKind::Count, ExactParams::Count),
        (ExactKind::Min, ExactParams::Min),
        (ExactKind::Max, ExactParams::Max),
        (ExactKind::Rate, ExactParams::Rate),
        (ExactKind::Increase, ExactParams::Increase),
    ] {
        let family = FieldDataType::ExactAggregate(kind.clone(), params);
        let build = || {
            Operator::per_group_summary_build(input(), family.clone(), 2, Some(1), vec![0]).unwrap()
        };
        let panes = [0, 1_000].map(|start| run(build(), vec![pane_input(start)]).remove(0));
        let as_objects =
            |pane: &Batch| Batch::try_new(pane.schema().clone(), pane.rows().to_vec()).unwrap();
        let state_schema = panes[0].schema().clone();
        let answer = |inputs: Vec<Batch>| {
            let merged = run(
                Operator::summary_merge(state_schema.clone(), 1, vec![0]).unwrap(),
                inputs,
            );
            let evaluation = Operator::evaluation(
                merged[0].schema().clone(),
                1,
                SummaryEvaluation::Exact(ExactEvaluation {
                    statistic: statistic(&kind),
                    lookback_ms: None,
                    offset_ms: 0,
                }),
            )
            .unwrap();
            let rows = run(evaluation, merged)[0].rows().to_vec();
            assert_eq!(rows.len(), 3, "{kind:?}: one row per series");
            format!("{rows:?}")
        };
        // Newest pane first: counters must merge in time order regardless.
        let records = answer(vec![panes[1].clone(), panes[0].clone()]);
        let objects = answer(vec![as_objects(&panes[1]), as_objects(&panes[0])]);
        let mixed = answer(vec![panes[1].clone(), as_objects(&panes[0])]);
        assert_eq!(records, objects, "{kind:?}");
        assert_eq!(records, mixed, "{kind:?}");
    }
}
