//! A Hydra-grouped Count-Min `SummaryAgg` compiles through the physical
//! planner and answers each group's item point counts from one shared grid.
use asap_executor::{
    operators::{Operator, RowWeight},
    physical_planner::{compile_node, CompiledPhysicalDAG, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{GroupKeys, Reduction};
use planner_types::ir::physical_export::{
    PhysicalASAPDAGNode, PhysicalASAPOperatorPayload as Payload,
};
use planner_types::ir::properties::*;
use planner_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use planner_types::ir::schema::DataType;
use planner_types::ir::schema::*;
use planner_types::ir::{ASAPOp, ExprSemantics, Predicate, ScalarExpr};
use planner_types::physical::Materialization;
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
    try_run(update, filter, batches).unwrap()
}

/// As [`run`], returning the first execution error.
fn try_run(
    update: SummaryUpdate,
    filter: Option<Predicate<usize>>,
    batches: Vec<Batch>,
) -> Result<BTreeMap<String, f64>, String> {
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
    let mut points = BTreeMap::new();
    for batch in batches {
        for row in batch.map_err(|e| e.to_string())?.to_rows() {
            match (&row[0], &row[1]) {
                (Value::Utf8(job), Value::Float64(point)) => {
                    points.insert(job.to_string(), *point);
                }
                other => panic!("unexpected row {other:?}"),
            }
        }
    }
    Ok(points)
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

fn weighted(weight_domain: WeightDomain) -> SummaryUpdate {
    SummaryUpdate {
        weight: SummaryInputExpr::Column(ColumnRef::Named("weight".into())),
        weight_domain,
        ..unit_count()
    }
}

fn non_negative() -> WeightDomain {
    WeightDomain::NonNegative {
        proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
    }
}

// A non-negative weight column sums each item's weight within its group
// (e.g. bytes per service per job), within the Count-Min bound over the
// stream's total weight; a split-stream merge answers as one build.
#[test]
fn weighted_hydra_cms_sums_item_weights_per_group() {
    let points = run(weighted(non_negative()), None, vec![batch(&ROWS)]);
    let total: f64 = ROWS.iter().map(|(_, _, w)| w).sum();
    let slack =
        std::f64::consts::E * total * (1. / f64::from(SHARED_COLUMNS) + 1. / f64::from(WIDTH));
    for job in ["api", "batch", "db"] {
        let truth: f64 = ROWS
            .iter()
            .filter(|(j, s, _)| *j == job && *s == "checkout")
            .map(|(_, _, w)| w)
            .sum();
        let point = points[job];
        assert!(point >= truth && point <= truth + slack, "{job}: {point}");
    }
    let (left, right) = ROWS.split_at(5);
    assert_eq!(
        run(
            weighted(non_negative()),
            None,
            vec![batch(left), batch(right)]
        ),
        points
    );
}

// A positive constant weight, `Constant(2.0)` with its `PositiveConstant`
// proof, adds 2 per row: each job's point count is twice the unit count's.
// A non-integer constant is refused at binding, since sketchlib's Hydra adds
// integer weights.
#[test]
fn positive_constant_weight_adds_the_constant_per_row() {
    let constant = |weight: f64| SummaryUpdate {
        weight: SummaryInputExpr::Constant(weight),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::PositiveConstant,
        },
        ..unit_count()
    };
    let unit = run(unit_count(), None, vec![batch(&ROWS)]);
    let doubled = run(constant(2.0), None, vec![batch(&ROWS)]);
    for job in ["api", "batch", "db"] {
        assert_eq!(doubled[job], 2.0 * unit[job], "{job}");
    }
    for weight in [0.0, -2.0, 2.5, f64::INFINITY] {
        let error = compile_node(&summary_agg(constant(weight), None), &[input_schema()])
            .err()
            .unwrap_or_else(|| panic!("constant weight {weight} accepted"));
        assert!(error.to_string().contains("HydraCms"), "{weight}: {error}");
    }
}

// The row weight is one serialized field: `{"Constant":2}` for a constant
// weight, `{"Column":2}` for a weight column. A deserialized constant above
// `i32::MAX` (asap_sketchlib's Hydra adds `i32` weights) is refused, as is
// one passed to the constructor.
#[test]
fn row_weight_serializes_as_one_field_and_rejects_constants_above_i32() {
    let constant = SummaryUpdate {
        weight: SummaryInputExpr::Constant(2.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::PositiveConstant,
        },
        ..unit_count()
    };
    let column = weighted(non_negative());
    for (update, expected) in [
        (constant, serde_json::json!({"Constant": 2})),
        (column, serde_json::json!({"Column": 2})),
    ] {
        let build = compile_node(&summary_agg(update, None), &[input_schema()]).unwrap();
        let wire = serde_json::to_value(&build).unwrap();
        let fields = &wire["kind"]["AllGroupsSummaryBuild"];
        assert_eq!(fields["row_weight"], expected, "{wire}");
        assert!(fields.get("weight").is_none() && fields.get("constant_weight").is_none());
        let restored: Operator = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), wire);
    }
    let build = |weight: u32| {
        Operator::all_groups_summary_build(
            input_schema(),
            family(),
            1,
            RowWeight::Constant(std::num::NonZeroU32::new(weight).unwrap()),
            vec![0],
        )
    };
    assert!(build(i32::MAX as u32).is_ok());
    let error = build(i32::MAX as u32 + 1)
        .err()
        .expect("constant above i32::MAX");
    assert!(error.to_string().contains("i32::MAX"), "{error}");
    let mut wire = serde_json::to_value(build(2).unwrap()).unwrap();
    wire["kind"]["AllGroupsSummaryBuild"]["row_weight"] =
        serde_json::json!({"Constant": i32::MAX as u32 + 1});
    let error = serde_json::from_value::<Operator>(wire)
        .err()
        .expect("deserialized constant above i32::MAX");
    assert!(error.to_string().contains("i32::MAX"), "{error}");
}

// A constant weight above `i32::MAX` (asap_sketchlib's Hydra adds `i32`
// weights) is refused by the kernel check, not only by the constructor.
#[test]
fn constant_weight_above_i32_is_refused_at_validation() {
    let update = SummaryUpdate {
        weight: SummaryInputExpr::Constant(2_147_483_648.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::PositiveConstant,
        },
        ..unit_count()
    };
    let error = asap_executor::capability::validate_summary_kernel(&family(), &update, &grouping())
        .expect_err("validation accepted a constant above i32::MAX");
    assert!(
        error.contains("HydraCms") && error.contains("i32::MAX"),
        "{error}"
    );
    let error = compile_node(&summary_agg(update, None), &[input_schema()])
        .err()
        .expect("constant above i32::MAX compiled");
    assert!(
        error.to_string().contains("HydraCms") && error.to_string().contains("i32::MAX"),
        "{error}"
    );
}

// A weight column of unknown sign is rejected at binding: a Count-Min point
// count only overestimates when no weight is negative.
#[test]
fn unknown_sign_weight_is_rejected() {
    let error = compile_node(
        &summary_agg(weighted(WeightDomain::UnknownOrSigned), None),
        &[input_schema()],
    )
    .err()
    .expect("unknown-sign weight");
    assert!(error.to_string().contains("non-negative"), "{error}");
}

// sketchlib's Hydra adds integer weights, so a negative, fractional or
// non-finite weight value fails the build with a clear error.
#[test]
fn invalid_weight_values_fail_the_build() {
    for (weight, expected) in [
        (-1.0, "non-negative"),
        (1.5, "integer"),
        (f64::NAN, "integer"),
        (f64::INFINITY, "integer"),
    ] {
        let rows = [("api", "checkout", 1.0), ("api", "checkout", weight)];
        let error = try_run(weighted(non_negative()), None, vec![batch(&rows)])
            .err()
            .unwrap_or_else(|| panic!("{weight} accepted"));
        assert!(error.contains(expected), "{weight}: {error}");
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
    let error = compile_node(&estimate(3, total, DataType::Int64), &[state])
        .err()
        .expect("group total");
    assert!(
        error
            .to_string()
            .contains("HydraCms does not answer a group total"),
        "{error}"
    );
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

// Groups of one build share one grid, so merging two builds over many groups
// merges the grids once and accounts for each grid once: 400 groups fit the
// default memory limit, which 400 grid copies (about 160 KiB each) exceed.
#[test]
fn many_groups_merge_one_grid_pair() {
    let jobs: Vec<String> = (0..400).map(|i| format!("job{i}")).collect();
    let rows: Vec<(&str, &str, f64)> = jobs
        .iter()
        .map(|job| (job.as_str(), "checkout", 1.0))
        .collect();
    // Every group is in both builds, so every group's state merges.
    let merged = try_run(unit_count(), None, vec![batch(&rows), batch(&rows)])
        .expect("merge within the memory limit");
    assert_eq!(merged.len(), 400);
    assert!(merged.values().all(|point| *point >= 2.0));
}
