//! A Hydra-grouped Count-Min `SummaryAgg` compiles through the physical
//! planner and answers each group's item point counts from one shared grid.
use asap_physical_operators::{
    dag::{
        operators::Operator,
        values::{Batch, SchemaRef, Value},
        Limits, RunContext, Scope,
    },
    physical_planner::{compile_node, CompiledPhysicalDAG, InputContract, Source},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::{
    PhysicalASAPDAGNode, PhysicalASAPOperatorPayload as Payload,
};
use planner_types::ir::{ASAPOp, ExprSemantics, Materialization, Predicate, ScalarExpr};
use planner_types::post_asap::*;
use planner_types::pre_asap::{
    ColumnRef, CompareOpKind, DataType, GroupKeys, Reduction, ScalarValue,
};
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

fn schema(fields: Vec<Field>) -> Schema {
    Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: None,
    }
}

fn node(id: usize, payload: Payload, output: Schema) -> PhysicalASAPDAGNode {
    PhysicalASAPDAGNode {
        id,
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Materialization::NotMaterialized,
        output_schema: output,
        guarantee: None,
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

fn unit_count() -> SummaryUpdate {
    SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named("service".into()))),
        weight: SummaryInputExpr::Constant(1.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
    }
}

/// The planner contract: item = the counted column, weight = unit count,
/// reduction = the group-by keys.
fn summary_agg(update: SummaryUpdate, filter: Option<Predicate<usize>>) -> PhysicalASAPDAGNode {
    node(
        1,
        Payload::ASAP(ASAPOp::SummaryAgg {
            child: 0,
            family: family(),
            input: update,
            reduction: Reduction::Reduce(GroupKeys::by(vec![0])),
            grouping: grouping(),
            filter,
        }),
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

fn estimate(id: usize, query: SketchStatistic, dtype: DataType) -> PhysicalASAPDAGNode {
    node(
        id,
        Payload::ASAP(ASAPOp::SummaryEstimate {
            summary_input: 1,
            query,
        }),
        schema(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("value", dtype, false),
        ]),
    )
}

fn point(item: &str) -> SketchStatistic {
    SketchStatistic::PointCount {
        key: ColumnRef::Named("service".into()),
        value: Some(item.into()),
    }
}

/// Compile `SummaryAgg` per input batch (merged when there are several),
/// then read each job's `checkout` frequency.
fn run(
    update: SummaryUpdate,
    filter: Option<Predicate<usize>>,
    batches: Vec<Batch>,
) -> BTreeMap<String, f64> {
    let input = input_schema();
    let build = compile_node(&summary_agg(update, filter), std::slice::from_ref(&input)).unwrap();
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
            &node(
                2,
                Payload::ASAP(ASAPOp::SummaryMerge { children: vec![1] }),
                (*state).clone(),
            ),
            std::slice::from_ref(&state),
        )
        .unwrap();
        operators.insert(201, (vec![200], merge));
        201
    };
    let read = compile_node(
        &estimate(3, point("checkout"), DataType::Float64),
        std::slice::from_ref(&state),
    )
    .unwrap();
    operators.insert(300, (vec![merged], read));
    let compiled = CompiledPhysicalDAG::from_operators(contracts, operators, vec![300]).unwrap();
    let dag = compiled.instantiate(sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let batches = block_on(
        dag.execute(&[300], context)
            .unwrap()
            .remove(0)
            .collect::<Vec<_>>(),
    );
    batches
        .into_iter()
        .flat_map(|batch| batch.unwrap().rows().to_vec())
        .map(|row| match (&row[0], &row[1]) {
            (Value::Utf8(job), Value::Float64(point)) => (job.to_string(), *point),
            other => panic!("unexpected row {other:?}"),
        })
        .collect()
}

fn exact(job: &str, service: &str) -> f64 {
    ROWS.iter()
        .filter(|(j, s, _)| *j == job && *s == service)
        .count() as f64
}

/// Every estimate is at least the exact value and within the Count-Min
/// additive slack `e·N/w` of both Hydra levels (shared cell and inner sketch).
fn assert_within_cms_bound(points: &BTreeMap<String, f64>) {
    let total = ROWS.len() as f64;
    let slack =
        std::f64::consts::E * total * (1. / f64::from(SHARED_COLUMNS) + 1. / f64::from(WIDTH));
    assert_eq!(points.len(), 3);
    for job in ["api", "batch", "db"] {
        let (point, truth) = (points[job], exact(job, "checkout"));
        assert!(point >= truth && point <= truth + slack, "{job}: {point}");
    }
}

// A unit-count HydraCms SummaryAgg compiles, runs, and answers per-group item
// frequencies within the CMS bound.
#[test]
fn unit_count_hydra_cms_answers_group_item_counts() {
    assert_within_cms_bound(&run(unit_count(), None, vec![batch(&ROWS)]));
}

// Merging two builds over a split stream answers exactly as one build over
// the whole stream.
#[test]
fn hydra_cms_merges_like_one_build() {
    let whole = run(unit_count(), None, vec![batch(&ROWS)]);
    let (left, right) = ROWS.split_at(5);
    assert_eq!(
        run(unit_count(), None, vec![batch(left), batch(right)]),
        whole
    );
}

// HydraCms counts rows: a weight column, signed or not, is rejected at binding.
#[test]
fn weighted_update_is_rejected() {
    for weight_domain in [
        WeightDomain::UnknownOrSigned,
        WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    ] {
        let update = SummaryUpdate {
            weight: SummaryInputExpr::Column(ColumnRef::Named("weight".into())),
            weight_domain,
            ..unit_count()
        };
        assert!(compile_node(&summary_agg(update, None), &[input_schema()]).is_err());
    }
}

// A group's total (a bare point count) needs a group-mass query that
// asap_sketchlib's Hydra does not offer yet, so it is rejected at binding
// rather than computed from sketchlib internals.
#[test]
fn group_total_is_rejected_until_sketchlib_offers_it() {
    let state = compile_node(&summary_agg(unit_count(), None), &[input_schema()])
        .unwrap()
        .schema();
    let total = SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    };
    assert!(compile_node(&estimate(3, total, DataType::Int64), &[state]).is_err());
}

// A filtered HydraCms build updates the grid only from rows where the filter
// is true; every group keeps its row, and `batch`, with no matching row,
// reads no checkout.
#[test]
fn filtered_hydra_cms_counts_matching_rows_and_keeps_groups() {
    // weight < 5 drops ("batch", "checkout", 7), ("db", "vacuum", 6).
    let filter = Predicate(ScalarExpr::Compare {
        semantics: ExprSemantics::Sql,
        op: CompareOpKind::Lt,
        left: Box::new(ScalarExpr::Column(2)),
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(5.0))),
    });
    let points = run(unit_count(), Some(filter), vec![batch(&ROWS)]);
    assert_eq!(points.len(), 3);
    assert_eq!(points["batch"], 0.0);
    assert!(points["api"] >= 2.0 && points["db"] >= 1.0);
}
