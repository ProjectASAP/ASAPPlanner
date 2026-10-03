//! One native UnivMon build supplies the three statistics in design Example 2.
use asap_physical_operators::{
    operators::{Operator, SummaryEvaluation},
    plan::{PhysicalDAG, PhysicalOperator},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{
        Field, FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SketchStatistic,
    },
    pre_asap::DataType,
};
use std::sync::Arc;

fn family() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size: 64,
                sketch_rows: 5,
                sketch_cols: 128,
                layers: 4,
            },
        ),
        GroupingStrategy::default(),
    )
}

fn value(row: &[Value]) -> f64 {
    match row {
        [Value::Float64(value)] => *value,
        other => panic!("expected one floating point result, got {other:?}"),
    }
}

#[test]
fn one_univmon_state_answers_distinct_l2_and_entropy() -> Result<(), Error> {
    let input: SchemaRef = Arc::new(planner_types::pre_asap::Schema::new(vec![Field::plain(
        "value",
        DataType::Float64,
        false,
    )]));
    let batch = Batch::try_new(
        input.clone(),
        [1.0, 1.0, 2.0, 2.0, 2.0]
            .into_iter()
            .map(|value| vec![Value::Float64(value)])
            .collect(),
    )?;
    let state = Operator::summary_build(input, family(), 0, None, vec![])?;
    let state_schema = state.output_schema();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(batch.schema().clone(), vec![batch])?,
    )?;
    dag.add(1, vec![0], state)?;
    dag.add(
        2,
        vec![1],
        Operator::evaluation(
            state_schema.clone(),
            0,
            SummaryEvaluation::Sketch(SketchStatistic::Cardinality),
        )?,
    )?;
    dag.add(
        3,
        vec![1],
        Operator::evaluation(
            state_schema.clone(),
            0,
            SummaryEvaluation::Sketch(SketchStatistic::FrequencyL2),
        )?,
    )?;
    dag.add(
        4,
        vec![1],
        Operator::evaluation(
            state_schema,
            0,
            SummaryEvaluation::Sketch(SketchStatistic::FrequencyEntropy),
        )?,
    )?;

    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )?;
    let outputs = block_on(futures::future::join_all(
        dag.execute(&[2, 3, 4], context)?
            .into_iter()
            .map(|stream| stream.collect::<Vec<_>>()),
    ));
    let batches: Vec<_> = outputs
        .into_iter()
        .map(|stream| stream.into_iter().collect::<Result<Vec<_>, _>>())
        .collect::<Result<_, _>>()?;
    assert!((value(batches[0][0].rows().first().unwrap()) - 2.0).abs() < 1.0);
    assert!((value(batches[1][0].rows().first().unwrap()) - 13.0_f64.sqrt()).abs() < 1.0);
    assert!(value(batches[2][0].rows().first().unwrap()) > 0.0);
    Ok(())
}
