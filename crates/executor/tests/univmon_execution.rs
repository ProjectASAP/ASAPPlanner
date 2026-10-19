//! One native UnivMon build supplies the three statistics of #509 Example 2:
//! a distinct count, an entropy and an L2 norm of one identity column.
use asap_executor::{
    operators::{Operator, SummaryEvaluation},
    physical_planner::compile_node,
    plan::{PhysicalDAG, PhysicalOperator},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{GroupKeys, Reduction};
use planner_types::ir::physical_export::{PhysicalASAPDAGNode, PhysicalASAPOperatorPayload};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, Schema};
use planner_types::ir::schema::{
    Field, FieldDataType, GroupingStrategy, NonNegativeWeightProof, SketchAlgorithm, SketchKind,
    SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use planner_types::ir::ASAPOp;
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
                distinct_failure_probability: None,
            },
        ),
        GroupingStrategy::default(),
    )
}

fn context() -> RunContext {
    RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap()
}

fn value(row: &[Value]) -> f64 {
    match row {
        [Value::Float64(value)] => *value,
        other => panic!("expected one floating point result, got {other:?}"),
    }
}

/// Build `build` over `batch` and read the three statistics from its state.
fn three_statistics(batch: Batch, build: Operator) -> Result<Vec<f64>, Error> {
    let state = build.output_schema();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(batch.schema().clone(), vec![batch])?,
    )?;
    dag.add(1, vec![0], build)?;
    for (id, statistic) in [
        (2, SketchStatistic::Cardinality),
        (3, SketchStatistic::FrequencyL2),
        (4, SketchStatistic::FrequencyEntropy),
    ] {
        dag.add(
            id,
            vec![1],
            Operator::evaluation(state.clone(), 0, SummaryEvaluation::Sketch(statistic))?,
        )?;
    }
    let outputs = block_on(futures::future::join_all(
        dag.execute(&[2, 3, 4], context())?
            .into_iter()
            .map(|stream| stream.collect::<Vec<_>>()),
    ));
    outputs
        .into_iter()
        .map(|stream| {
            let batches = stream.into_iter().collect::<Result<Vec<_>, _>>()?;
            Ok(value(&batches[0].to_rows()[0]))
        })
        .collect()
}

// One UnivMon state over Float64 samples answers distinct, L2 and entropy.
#[test]
fn one_univmon_state_answers_distinct_l2_and_entropy() -> Result<(), Error> {
    let input: SchemaRef = Arc::new(Schema::new(vec![Field::plain(
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
    let build = Operator::per_group_summary_build(input, family(), 0, None, vec![])?;
    let [distinct, l2, entropy] = three_statistics(batch, build)?[..] else {
        unreachable!()
    };
    assert!((distinct - 2.0).abs() < 0.01);
    assert!((l2 - 13.0_f64.sqrt()).abs() < 0.01);
    let expected = -0.4_f64 * 0.4_f64.log2() - 0.6 * 0.6_f64.log2();
    assert!((entropy - expected).abs() < 0.01, "{entropy}");
    Ok(())
}

// SQL source IPs and integer identifiers enter frequency state without
// numeric coercion; NULL identities are skipped.
#[test]
fn typed_frequency_keys_preserve_identity() -> Result<(), Error> {
    for (dtype, keys) in [
        (
            DataType::Utf8,
            [
                Value::Utf8("192.0.2.1".into()),
                Value::Utf8("192.0.2.2".into()),
            ],
        ),
        (
            DataType::Int64,
            [
                Value::Int64(9_007_199_254_740_992),
                Value::Int64(9_007_199_254_740_993),
            ],
        ),
        (DataType::Bool, [Value::Bool(false), Value::Bool(true)]),
    ] {
        let input: SchemaRef = Arc::new(Schema::new(vec![Field::plain("src_ip", dtype, true)]));
        let batch = Batch::try_new(
            input.clone(),
            vec![
                vec![keys[0].clone()],
                vec![keys[0].clone()],
                vec![keys[1].clone()],
                vec![keys[1].clone()],
                vec![Value::Null],
            ],
        )?;
        let build = Operator::per_group_summary_build(input, family(), 0, None, vec![])?;
        for (found, expected) in
            three_statistics(batch, build)?
                .into_iter()
                .zip([2.0, 8.0_f64.sqrt(), 1.0])
        {
            assert!((found - expected).abs() < 0.01, "{found} vs {expected}");
        }
    }
    Ok(())
}

// Pass 1's UnivMon update (each item column value is one unit occurrence)
// binds as a value-frequency build over that column, a Utf8 one included.
#[test]
fn planner_unit_frequency_update_binds_as_a_value_frequency_build() -> Result<(), Error> {
    let input: SchemaRef = Arc::new(Schema::new(vec![Field::plain(
        "src_ip",
        DataType::Utf8,
        false,
    )]));
    let state = Arc::new(Schema::new(vec![Field {
        table: None,
        name: "state".into(),
        dtype: family(),
        nullable: false,
    }]));
    let node = PhysicalASAPDAGNode {
        id: 1,
        payload: PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            family: family(),
            input: SummaryUpdate {
                item: Some(SummaryInputExpr::Column(ColumnRef::Named("src_ip".into()))),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            },
            reduction: Reduction::Reduce(GroupKeys::by(vec![])),
            grouping: GroupingStrategy::default(),
            filter: None,
        }),
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Default::default(),
        output_schema: (*state).clone(),
        guarantee: None,
    };
    let build = compile_node(&node, std::slice::from_ref(&input))?;
    let batch = Batch::try_new(
        input,
        ["a", "a", "b", "b"]
            .into_iter()
            .map(|key| vec![Value::Utf8(key.into())])
            .collect(),
    )?;
    let found = three_statistics(batch, build)?;
    assert!((found[0] - 2.0).abs() < 0.01, "{found:?}");
    Ok(())
}
