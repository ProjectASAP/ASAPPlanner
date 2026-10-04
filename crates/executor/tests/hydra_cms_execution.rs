//! A Hydra-grouped Count-Min SummaryAgg compiles through the physical planner
//! and answers per-group counts and item point counts (#580, W7).
use asap_executor::{
    dag::{
        operators::Operator,
        values::{Batch, SchemaRef, Value},
        Limits, RunContext, Scope,
    },
    physical_planner::{compile_node, CompiledPhysicalDAG, InputContract, Source},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::export::{
    LogicalASAPNodeId, PhysicalASAPDAGNode, PhysicalASAPOperatorPayload as Payload,
};
use planner_types::ir::operator::{GroupKeys, Reduction};
use planner_types::ir::properties::ExecutionDataState;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::*;
use std::{collections::BTreeMap, sync::Arc};

const WIDTH: u32 = 64;
const SHARED_COLUMNS: u32 = 64;

/// `(job, service, weight)` rows; jobs are the groups, services the items.
const ROWS: [(&str, &str, f64); 9] = [
    ("api", "checkout", 3.),
    ("api", "checkout", 2.),
    ("api", "search", 1.),
    ("api", "auth", 4.),
    ("batch", "checkout", 7.),
    ("batch", "export", 1.),
    ("db", "checkout", 1.),
    ("db", "vacuum", 6.),
    ("db", "vacuum", 2.),
];

fn grouping() -> GroupingStrategy {
    GroupingStrategy::SharedMultiSubpopulation {
        kind: HydraKind::HydraCms,
        params: HydraParams::HydraCms {
            width: WIDTH,
            depth: 3,
            shared_rows: 3,
            shared_columns: SHARED_COLUMNS,
        },
    }
}

fn family() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Cms,
            SketchParams::Cms {
                width: WIDTH,
                depth: 3,
            },
        ),
        grouping(),
    )
}

fn schema(fields: Vec<Field<FieldDataType>>) -> Schema {
    Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: None,
    }
}

fn node(id: u32, payload: Payload, output: Schema) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id: LogicalASAPNodeId(id),
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        output_schema: output,
        guarantee: None,
        coverage: None,
    }
}

fn input_schema() -> SchemaRef {
    Arc::new(schema(vec![
        Field::plain("job", DataType::Utf8, false),
        Field::plain("service", DataType::Utf8, false),
        Field::plain("weight", DataType::Float64, false),
    ]))
}

fn batch(rows: &[(&str, &str, f64)]) -> Batch {
    Batch::try_new(
        input_schema(),
        rows.iter()
            .map(|(job, service, weight)| {
                vec![
                    Value::Utf8((*job).into()),
                    Value::Utf8((*service).into()),
                    Value::Float64(*weight),
                ]
            })
            .collect(),
    )
    .unwrap()
}

/// The planner contract: item = the counted column, weight = unit count or a
/// non-negative weight column, reduction = the group-by keys.
fn summary_agg(update: SummaryUpdate) -> PhysicalASAPDAGNode {
    node(
        1,
        Payload::SummaryAgg {
            family: family(),
            input: update,
            reduction: Reduction::Reduce(GroupKeys::by(vec![0])),
            grouping: grouping(),
            filter: None,
        },
        schema(vec![
            Field::plain("job", DataType::Utf8, false),
            Field {
                name: "state".into(),
                dtype: family(),
                nullable: false,
                table: None,
            },
        ]),
    )
}

fn estimate(id: u32, query: SketchStatistic, dtype: DataType) -> PhysicalASAPDAGNode {
    node(
        id,
        Payload::SummaryEstimate { query },
        schema(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("value", dtype, false),
        ]),
    )
}

/// Compile `SummaryAgg` per input batch (merged when there are several),
/// then both readouts; return `(job → count, job → checkout frequency)`.
fn run(
    update: SummaryUpdate,
    batches: Vec<Batch>,
) -> (BTreeMap<String, i64>, BTreeMap<String, f64>) {
    let input = input_schema();
    let agg = summary_agg(update);
    let build = compile_node(&agg, std::slice::from_ref(&input)).unwrap();
    let state = build.schema();
    let mut operators = BTreeMap::new();
    let mut contracts = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let mut states = vec![];
    for (i, batch) in batches.into_iter().enumerate() {
        let (source, built) = (i as u64, 100 + i as u64);
        contracts.insert(source, InputContract::bounded(input.clone()));
        sources.insert(
            source,
            Box::new(Operator::source(input.clone(), vec![batch]).unwrap()) as Source<'_>,
        );
        operators.insert(built, (vec![source], build.clone()));
        states.push(built);
    }
    let merged = if states.len() == 1 {
        states[0]
    } else {
        let union = Operator::union(state.clone(), states.len()).unwrap();
        operators.insert(200, (states, union));
        let merge = compile_node(
            &node(2, Payload::SummaryMerge, (*state).clone()),
            std::slice::from_ref(&state),
        )
        .unwrap();
        operators.insert(201, (vec![200], merge));
        201
    };
    let count = estimate(
        3,
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        DataType::Int64,
    );
    let point = estimate(
        4,
        SketchStatistic::PointCount {
            key: ColumnRef::Named("service".into()),
            value: Some("checkout".into()),
        },
        DataType::Float64,
    );
    for (id, node) in [(300, &count), (301, &point)] {
        let operator = compile_node(node, std::slice::from_ref(&state)).unwrap();
        operators.insert(id, (vec![merged], operator));
    }
    let compiled =
        CompiledPhysicalDAG::from_operators(contracts, operators, vec![300, 301]).unwrap();
    let dag = compiled.instantiate(sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let outputs = block_on(futures::future::join_all(
        dag.execute(&[300, 301], context)
            .unwrap()
            .into_iter()
            .map(|stream| stream.collect::<Vec<_>>()),
    ));
    let mut outputs = outputs.into_iter().map(|stream| {
        stream
            .into_iter()
            .flat_map(|batch| batch.unwrap().rows().to_vec())
            .collect::<Vec<_>>()
    });
    let rows = |rows: Vec<Vec<Value>>| {
        rows.into_iter()
            .map(|row| match &row[0] {
                Value::Utf8(job) => (job.to_string(), row[1].clone()),
                other => panic!("unexpected group {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let counts = rows(outputs.next().unwrap())
        .into_iter()
        .map(|(job, value)| match value {
            Value::Int64(count) => (job, count),
            other => panic!("count must be Int64, got {other:?}"),
        })
        .collect();
    let points = rows(outputs.next().unwrap())
        .into_iter()
        .map(|(job, value)| match value {
            Value::Float64(point) => (job, point),
            other => panic!("point must be Float64, got {other:?}"),
        })
        .collect();
    (counts, points)
}

fn exact(weighted: bool, job: &str, service: Option<&str>) -> f64 {
    ROWS.iter()
        .filter(|(j, s, _)| *j == job && service.is_none_or(|service| service == *s))
        .map(|(_, _, w)| if weighted { *w } else { 1. })
        .sum()
}

/// Every estimate is at least the exact value and within the Count-Min
/// additive slack `e·N/w` of both Hydra levels (shared cell and inner sketch).
fn assert_within_cms_bound(
    weighted: bool,
    counts: &BTreeMap<String, i64>,
    points: &BTreeMap<String, f64>,
) {
    let total: f64 = ROWS
        .iter()
        .map(|(_, _, w)| if weighted { *w } else { 1. })
        .sum();
    let slack =
        std::f64::consts::E * total * (1. / f64::from(SHARED_COLUMNS) + 1. / f64::from(WIDTH));
    assert_eq!(counts.len(), 3);
    assert_eq!(points.len(), 3);
    for job in ["api", "batch", "db"] {
        let (count, true_count) = (counts[job] as f64, exact(weighted, job, None));
        assert!(
            count >= true_count && count <= true_count + slack,
            "{job}: {count}"
        );
        let (point, true_point) = (points[job], exact(weighted, job, Some("checkout")));
        assert!(
            point >= true_point && point <= true_point + slack,
            "{job}: {point}"
        );
    }
}

// A unit-count HydraCms SummaryAgg compiles, runs, and answers per-group
// counts and per-group item frequencies within the CMS bound.
#[test]
fn unit_count_hydra_cms_answers_group_counts_and_points() {
    let update = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named("service".into()))),
        weight: SummaryInputExpr::Constant(1.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
    };
    let (counts, points) = run(update, vec![batch(&ROWS)]);
    assert_within_cms_bound(false, &counts, &points);
}

// A non-negative weight column feeds HydraCms, and merging two builds over a
// split stream answers exactly as one build over the whole stream.
#[test]
fn weighted_hydra_cms_merges_like_one_build() {
    let update = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named("service".into()))),
        weight: SummaryInputExpr::Column(ColumnRef::Named("weight".into())),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::CounterSamples,
        },
    };
    let whole = run(update.clone(), vec![batch(&ROWS)]);
    assert_within_cms_bound(true, &whole.0, &whole.1);
    let (left, right) = ROWS.split_at(5);
    assert_eq!(run(update, vec![batch(left), batch(right)]), whole);
}

// HydraCms needs a non-negative weight proof; a signed weight is rejected at binding.
#[test]
fn signed_weight_is_rejected() {
    let update = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named("service".into()))),
        weight: SummaryInputExpr::Column(ColumnRef::Named("weight".into())),
        weight_domain: WeightDomain::UnknownOrSigned,
    };
    assert!(compile_node(&summary_agg(update), &[input_schema()]).is_err());
}
