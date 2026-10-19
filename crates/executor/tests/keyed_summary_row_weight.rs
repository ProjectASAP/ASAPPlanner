//! A per-group keyed summary build (a CMS or Count Sketch with heap per
//! group) adds each row's `RowWeight` to the row's item: the value of a
//! weight column, or a constant such as 1 for a row count.
use asap_executor::{
    dag::{
        operators::{Operator, RowWeight},
        values::{Batch, SchemaRef, Value},
        Limits, PhysicalDAG, RunContext, Scope,
    },
    physical_planner::compile_node,
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{GroupKeys, Reduction};
use planner_types::ir::physical_export::{
    PhysicalASAPDAGNode, PhysicalASAPOperatorPayload as Payload,
};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::*;
use planner_types::ir::ASAPOp;
use planner_types::physical::Materialization;
use std::sync::Arc;

/// `(job, service)` rows: jobs are the groups, services the items.
const ROWS: [(&str, &str); 6] = [
    ("api", "checkout"),
    ("api", "checkout"),
    ("api", "search"),
    ("batch", "export"),
    ("batch", "export"),
    ("batch", "export"),
];

fn heap_family() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 1024,
                depth: 3,
                heap_size: 8,
            },
        ),
        GroupingStrategy::PerSubpopulationInstance,
    )
}

fn schema(fields: Vec<Field>) -> SchemaRef {
    Arc::new(Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: None,
    })
}

/// `(job, service)` input schema, plus a `weight` column of `weight` type
/// when given.
fn input_schema(weight: Option<(DataType, bool)>) -> SchemaRef {
    let mut fields = vec![
        Field::plain("job", DataType::Utf8, false),
        Field::plain("service", DataType::Utf8, false),
    ];
    if let Some((dtype, nullable)) = weight {
        fields.push(Field::plain("weight", dtype, nullable));
    }
    schema(fields)
}

/// `ROWS` with each row's `weight` value appended, when given.
fn batch(input: &SchemaRef, weight: Option<Value>) -> Batch {
    Batch::try_new(
        input.clone(),
        ROWS.iter()
            .map(|(job, service)| {
                let mut row = vec![Value::Utf8((*job).into()), Value::Utf8((*service).into())];
                row.extend(weight.clone());
                row
            })
            .collect(),
    )
    .unwrap()
}

/// Run `build` over `batch` and read each group's top items as sorted
/// `(job, service, score)` rows.
fn top_items(build: Operator, input: SchemaRef, batch: Batch) -> Vec<(String, String, f64)> {
    let output = schema(vec![
        Field::plain("job", DataType::Utf8, false),
        Field::plain("service", DataType::Utf8, false),
        Field::plain("score", DataType::Float64, false),
    ]);
    let evaluation = Operator::keyed_evaluation(build.schema(), 1, 8, output).unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], Operator::source(input, vec![batch]).unwrap())
        .unwrap();
    dag.add(1, vec![0], build).unwrap();
    dag.add(2, vec![1], evaluation).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let mut rows = block_on(async {
        let mut rows = Vec::new();
        let mut stream = dag.execute(&[2], context).unwrap().remove(0);
        while let Some(batch) = stream.next().await {
            rows.extend(batch.unwrap().to_rows().iter().cloned());
        }
        rows
    })
    .into_iter()
    .map(|row| match (&row[0], &row[1], &row[2]) {
        (Value::Utf8(job), Value::Utf8(service), Value::Float64(score)) => {
            (job.to_string(), service.to_string(), *score)
        }
        other => panic!("unexpected row {other:?}"),
    })
    .collect::<Vec<_>>();
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows
}

/// The count of each `(job, service)` in `ROWS`, times `weight`.
fn counts(weight: f64) -> Vec<(String, String, f64)> {
    [
        ("api", "checkout", 2.),
        ("api", "search", 1.),
        ("batch", "export", 3.),
    ]
    .map(|(job, service, count)| (job.to_string(), service.to_string(), count * weight))
    .to_vec()
}

// A count-ranked heap (`topk by(job)(k, count by(job, service)(...))`)
// updates by the unit count: `SummaryUpdate.weight = Constant(1.0)`. At query
// time it binds to a keyed build whose row weight is the constant 1, not a
// materialized column of ones.
#[test]
fn query_time_count_heap_binds_a_constant_row_weight() {
    let input = input_schema(None);
    let node = PhysicalASAPDAGNode {
        id: 1,
        payload: Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            family: heap_family(),
            input: SummaryUpdate {
                item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                    ColumnRef::Named("service".into()),
                )])),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            },
            reduction: Reduction::Reduce(GroupKeys::by(vec![0])),
            grouping: GroupingStrategy::PerSubpopulationInstance,
            filter: None,
        }),
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Materialization::NotMaterialized,
        output_schema: (*schema(vec![
            Field::plain("job", DataType::Utf8, false),
            Field {
                name: "state".into(),
                dtype: heap_family(),
                nullable: false,
                table: None,
            },
        ]))
        .clone(),
        guarantee: None,
    };
    let build = compile_node(&node, std::slice::from_ref(&input)).unwrap();
    let wire = serde_json::to_value(&build).unwrap();
    assert_eq!(
        wire["kind"]["PerGroupKeyedSummaryBuild"]["row_weight"],
        serde_json::json!({"Constant": 1}),
        "{wire}"
    );
    assert_eq!(
        top_items(build, input.clone(), batch(&input, None)),
        counts(1.)
    );
}

/// A keyed build of `ROWS` by `job`, items `service`, adding `row_weight`.
fn keyed_build(input: &SchemaRef, row_weight: RowWeight) -> Operator {
    Operator::per_group_keyed_summary_build(
        input.clone(),
        heap_family(),
        vec![1],
        row_weight,
        vec![0],
    )
    .unwrap()
}

// A constant row weight adds the same as a column holding that constant:
// `Constant(3)` over `ROWS` equals `Column(2)` over a Float64 column of 3.0,
// and both are 3 x each item's row count.
#[test]
fn constant_row_weight_equals_a_column_of_that_constant() {
    let plain = input_schema(None);
    let constant = top_items(
        keyed_build(
            &plain,
            RowWeight::Constant(std::num::NonZeroU32::new(3).unwrap()),
        ),
        plain.clone(),
        batch(&plain, None),
    );
    let weighted = input_schema(Some((DataType::Float64, false)));
    let column = top_items(
        keyed_build(&weighted, RowWeight::Column(2)),
        weighted.clone(),
        batch(&weighted, Some(Value::Float64(3.))),
    );
    assert_eq!(constant, column);
    assert_eq!(constant, counts(3.));
}

// An Int64 weight column adds the same as a Float64 column of equal values;
// an Int64 weight beyond 2^53 (no exact Float64) is refused, not rounded.
#[test]
fn int64_weight_column_equals_float64_column() {
    let int64 = input_schema(Some((DataType::Int64, false)));
    assert_eq!(
        top_items(
            keyed_build(&int64, RowWeight::Column(2)),
            int64.clone(),
            batch(&int64, Some(Value::Int64(2))),
        ),
        counts(2.)
    );
    // ±2^53 is the largest Int64 magnitude with an exact Float64, so it
    // builds: 2^53 on the Count-Min heap (non-negative weights only), -2^53
    // on the Count Sketch heap, which takes signed weights.
    let limit = 1i64 << 53;
    assert_eq!(
        top_items(
            keyed_build(&int64, RowWeight::Column(2)),
            int64.clone(),
            batch(&int64, Some(Value::Int64(limit))),
        ),
        counts(limit as f64)
    );
    let count_sketch = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width: 1024,
                depth: 3,
                heap_size: 8,
            },
        ),
        GroupingStrategy::PerSubpopulationInstance,
    );
    let signed = Operator::per_group_keyed_summary_build(
        int64.clone(),
        count_sketch,
        vec![1],
        RowWeight::Column(2),
        vec![0],
    )
    .unwrap();
    assert_eq!(
        top_items(
            signed,
            int64.clone(),
            batch(&int64, Some(Value::Int64(-limit)))
        )
        .len(),
        3,
        "-2^53 builds and every item is read back"
    );
    let build = keyed_build(&int64, RowWeight::Column(2));
    let evaluation = Operator::keyed_evaluation(
        build.schema(),
        1,
        8,
        schema(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("service", DataType::Utf8, false),
            Field::plain("score", DataType::Float64, false),
        ]),
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(
            int64.clone(),
            vec![batch(&int64, Some(Value::Int64((1 << 53) + 1)))],
        )
        .unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], build).unwrap();
    dag.add(2, vec![1], evaluation).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let error = block_on(dag.execute(&[2], context).unwrap().remove(0).next())
        .unwrap()
        .err()
        .expect("Int64 weight beyond 2^53 accepted");
    assert!(error.to_string().contains("no exact Float64"), "{error}");
    // A Utf8 weight column is refused when the build is constructed.
    let text = input_schema(Some((DataType::Utf8, false)));
    let error = Operator::per_group_keyed_summary_build(
        text,
        heap_family(),
        vec![1],
        RowWeight::Column(2),
        vec![0],
    )
    .err()
    .expect("Utf8 weight accepted");
    assert!(error.to_string().contains("Float64 or Int64"), "{error}");
}

// A NULL in a nullable weight column adds nothing, as with SQL `SUM`: no
// item is counted, while each row's group still gets a state.
#[test]
fn null_weight_adds_nothing() {
    let nullable = input_schema(Some((DataType::Float64, true)));
    let nulls = batch(&nullable, Some(Value::Null));
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(nullable.clone(), vec![nulls.clone()]).unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], keyed_build(&nullable, RowWeight::Column(2)))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let states = block_on(dag.execute(&[1], context).unwrap().remove(0).next())
        .unwrap()
        .unwrap();
    assert_eq!(states.to_rows().len(), 2, "one state per job");
    assert_eq!(
        top_items(
            keyed_build(&nullable, RowWeight::Column(2)),
            nullable,
            nulls
        ),
        vec![]
    );
}
