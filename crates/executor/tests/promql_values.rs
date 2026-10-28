//! Compile, persist and rebind dynamic-label computation without deployment lowering.
use asap_executor::expressions::binary::{BinaryOpKind, BinaryOperator};
use asap_executor::sources::InMemoryDataStore;
use std::sync::Arc;

use asap_executor::{
    operators::{Expression, Operator, Reduction, SortKey},
    physical_planner::{promql_values::*, CompiledPhysicalDAG},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{AggIntent, GroupKeys};
use planner_types::ir::scalar::ColumnRef;

use std::collections::BTreeMap;

/// Lowered PromQL operators over input nodes `0..inputs`, which a run feeds
/// with `Operator::source`s.
#[derive(Clone)]
struct Computation {
    operators: BTreeMap<u64, (Vec<u64>, Operator)>,
    root: u64,
}

/// `operators` chained over one input (node 0): operator `i` is node `i + 1`.
fn unary(operators: Vec<Operator>) -> Computation {
    let root = operators.len() as u64;
    Computation {
        operators: operators
            .into_iter()
            .enumerate()
            .map(|(i, op)| ((i + 1) as u64, (vec![i as u64], op)))
            .collect(),
        root,
    }
}

/// `operator` (node 2) over inputs 0 and 1, in operand order.
fn binary_of(operator: Operator) -> Computation {
    Computation {
        operators: BTreeMap::from([(2, (vec![0, 1], operator))]),
        root: 2,
    }
}

/// PromQL `range_function(m[w])` over a matrix: dropping the metric name
/// unless `preserve_metric_name`.
fn compile_temporal(
    intent: &AggIntent<ColumnRef>,
    preserve_metric_name: bool,
) -> Result<Computation, Error> {
    let mut operators = vec![Operator::range_window(intent.clone())?];
    if !preserve_metric_name {
        operators.push(Operator::project(
            vector_schema(),
            vec![
                (
                    "labels".into(),
                    Expression::LabelSet {
                        column: 0,
                        labels: vec![],
                        without: true,
                    },
                ),
                ("value".into(), Expression::Column(1)),
            ],
        )?);
    }
    Ok(unary(operators))
}

/// `histogram_quantile(q, buckets)`: `q` input 0, the buckets input 1.
fn compile_histogram_quantile() -> Computation {
    binary_of(Operator::histogram_quantile())
}

/// A PromQL binary operation; input 0 the left operand, 1 the right.
fn compile_binary(
    operator: &BinaryOperator,
    return_bool: bool,
    left_scalar: bool,
    right_scalar: bool,
) -> Result<Computation, Error> {
    let schema = |scalar| match scalar {
        true => scalar_schema(),
        false => vector_schema(),
    };
    Ok(binary_of(Operator::vector_binary(
        schema(left_scalar),
        schema(right_scalar),
        operator.clone(),
        return_bool,
    )?))
}

/// The vector's rows with a `group` column: their label set under `grouping`.
fn grouped(grouping: &GroupKeys<ColumnRef>) -> Result<Operator, Error> {
    let labels = grouping
        .keys()
        .iter()
        .map(|key| match key {
            ColumnRef::Named(label) => Ok(label.clone()),
            _ => Err(Error::Invalid(
                "vector grouping requires label names".into(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Operator::project(
        vector_schema(),
        vec![
            ("labels".into(), Expression::Column(0)),
            ("value".into(), Expression::Column(1)),
            (
                "group".into(),
                Expression::LabelSet {
                    column: 0,
                    labels,
                    without: grouping.is_without(),
                },
            ),
        ],
    )
}

/// The `(labels, value)` vector of `input`'s columns `labels` and `value`.
fn vector_output(input: SchemaRef, labels: usize, value: usize) -> Result<Operator, Error> {
    Operator::project(
        input,
        vec![
            ("labels".into(), Expression::Column(labels)),
            ("value".into(), Expression::ExactFloat64(value)),
        ],
    )
}

/// A PromQL vector aggregation `intent by|without (grouping) (v)`.
fn compile_aggregate(
    intent: &AggIntent<ColumnRef>,
    grouping: &GroupKeys<ColumnRef>,
) -> Result<Computation, Error> {
    let project = grouped(grouping)?;
    let reduction = match intent {
        AggIntent::Sum { .. } => Reduction::Sum(1),
        AggIntent::Avg { .. } => Reduction::Avg(1),
        AggIntent::Count { .. } => Reduction::Count,
        AggIntent::Min { .. } => Reduction::Min(1),
        AggIntent::Max { .. } => Reduction::Max(1),
        _ => return Err(Error::Invalid("unsupported vector aggregate".into())),
    };
    let aggregate =
        Operator::aggregate(project.schema(), vec![2], vec![("value".into(), reduction)])?;
    let output = vector_output(aggregate.schema(), 0, 1)?;
    Ok(unary(vec![project, aggregate, output]))
}

/// A sort of the vector by value within each `grouping` group.
fn compile_sort(descending: bool, grouping: &GroupKeys<ColumnRef>) -> Result<Computation, Error> {
    let project = grouped(grouping)?;
    let sort = Operator::sort(
        project.schema(),
        vec![SortKey {
            column: 1,
            descending,
            nulls_first: false,
        }],
        vec![2],
    )?;
    let output = vector_output(sort.schema(), 0, 1)?;
    Ok(unary(vec![project, sort, output]))
}

/// The first `n` rows after `offset` of each `grouping` group.
fn compile_limit(
    n: u64,
    offset: u64,
    grouping: &GroupKeys<ColumnRef>,
) -> Result<Computation, Error> {
    let project = grouped(grouping)?;
    let limit = Operator::limit(project.schema(), n, offset, vec![2])?;
    let output = vector_output(limit.schema(), 0, 1)?;
    Ok(unary(vec![project, limit, output]))
}

/// PromQL `scalar(v)`.
fn compile_vector_to_scalar() -> Result<Computation, Error> {
    Ok(unary(vec![Operator::vector_to_scalar(vector_schema(), 1)?]))
}

/// Stored exact states of `family`, per series: `(labels, state)`.
fn exact_state_schema(family: planner_types::ir::schema::FieldDataType) -> SchemaRef {
    let mut schema = (*vector_schema()).clone();
    schema.fields[1].dtype = family;
    Arc::new(schema)
}

/// Merge stored exact panes per series and finalize them; a counter (`Rate`,
/// `Increase`) over the `lookback_ms` window ending at the evaluation time.
fn compile_exact_evaluation(
    family: planner_types::ir::schema::FieldDataType,
    lookback_ms: i64,
    preserve_metric_name: bool,
) -> Result<Computation, Error> {
    use asap_executor::operators::SummaryEvaluation;
    use asap_executor::summary_kernels::exact::ExactEvaluation;
    use planner_types::ir::schema::{ExactKind, FieldDataType};
    let FieldDataType::ExactAggregate(statistic, _) = family else {
        return Err(Error::Invalid(
            "exact evaluation requires an exact family".into(),
        ));
    };
    let is_counter = matches!(statistic, ExactKind::Rate | ExactKind::Increase);
    let merge = Operator::summary_merge(exact_state_schema(family), 1, vec![0])?;
    let evaluation = Operator::evaluation(
        merge.schema(),
        1,
        SummaryEvaluation::Exact(ExactEvaluation {
            statistic,
            lookback_ms: is_counter.then_some(lookback_ms),
            offset_ms: 0,
        }),
    )?;
    let project = Operator::project(
        evaluation.schema(),
        vec![
            (
                "labels".into(),
                if preserve_metric_name {
                    Expression::Column(0)
                } else {
                    Expression::LabelSet {
                        column: 0,
                        labels: vec![],
                        without: true,
                    }
                },
            ),
            ("value".into(), Expression::ExactFloat64(1)),
        ],
    )?;
    Ok(unary(vec![merge, evaluation, project]))
}

fn row(labels: &[(&str, &str)], value: f64) -> Vec<Value> {
    vec![
        Value::Map(
            labels
                .iter()
                .map(|(k, v)| (Value::Utf8((*k).into()), Value::Utf8((*v).into())))
                .collect::<Vec<_>>()
                .into(),
        ),
        Value::Float64(value),
    ]
}
fn run(computation: Computation, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    run_inputs(
        computation,
        vec![Batch::try_new(vector_schema(), rows).unwrap()],
    )
    .unwrap()
}
/// `computation` with input `i` the source of `batches[i]`, at time 0.
fn run_inputs(computation: Computation, batches: Vec<Batch>) -> Result<Vec<Vec<Value>>, Error> {
    let mut operators = computation.operators;
    for (id, batch) in batches.into_iter().enumerate() {
        let source = Operator::source(batch.schema().clone(), vec![batch]).unwrap();
        operators.insert(id as u64, (vec![], source));
    }
    let dag = CompiledPhysicalDAG::from_operators(operators, vec![computation.root]).unwrap();
    execute(&dag)
}
/// The rows of `dag`'s root at time 0, over a store with no data.
fn execute(dag: &CompiledPhysicalDAG) -> Result<Vec<Vec<Value>>, Error> {
    let bound = dag
        .instantiate(Arc::new(InMemoryDataStore::default()))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = bound.execute(dag.roots(), context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(batch?.to_rows().iter().cloned());
        }
        Ok(rows)
    })
}
fn equal_rows(actual: Vec<Vec<Value>>, expected: Vec<Vec<Value>>) {
    let mut actual = actual
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>();
    let mut expected = expected
        .into_iter()
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn grouping_preserves_unenumerated_labels_and_empty_label_semantics() {
    let rows = vec![
        row(&[("__name__", "m"), ("instance", "a"), ("job", "api")], 1.),
        row(&[("__name__", "m"), ("instance", "b"), ("job", "api")], 2.),
        row(&[("instance", "c"), ("job", "")], 4.),
        row(&[("instance", "d")], 8.),
    ];
    equal_rows(
        run(
            compile_aggregate(
                &AggIntent::Sum { col: None },
                &GroupKeys::without(vec![ColumnRef::Named("instance".into())]),
            )
            .unwrap(),
            rows.clone(),
        ),
        vec![row(&[("job", "api")], 3.), row(&[], 12.)],
    );
    equal_rows(
        run(
            compile_aggregate(
                &AggIntent::Count {
                    accuracy: planner_types::ir::properties::AccuracyTarget::exact(),
                },
                &GroupKeys::by(vec![ColumnRef::Named("job".into())]),
            )
            .unwrap(),
            rows,
        ),
        vec![row(&[("job", "api")], 2.), row(&[], 2.)],
    );
}

#[test]
fn ranking_and_grouped_limit_preserve_full_selected_series() {
    let grouping = GroupKeys::by(vec![ColumnRef::Named("job".into())]);
    let rows = vec![
        row(&[("instance", "a"), ("job", "api")], 1.),
        row(&[("instance", "b"), ("job", "api")], 3.),
        row(&[("instance", "c"), ("job", "worker")], 2.),
    ];
    let sorted = run(compile_sort(true, &grouping).unwrap(), rows);
    let selected = run(compile_limit(1, 0, &grouping).unwrap(), sorted);
    equal_rows(
        selected,
        vec![
            row(&[("instance", "b"), ("job", "api")], 3.),
            row(&[("instance", "c"), ("job", "worker")], 2.),
        ],
    );
    assert!(run(compile_limit(0, 0, &grouping).unwrap(), vec![row(&[], 1.)]).is_empty());
}

#[test]
fn empty_vector_aggregation_stays_empty() {
    assert!(run(
        compile_aggregate(&AggIntent::Sum { col: None }, &GroupKeys::default()).unwrap(),
        vec![]
    )
    .is_empty());
    let scalar = run(compile_vector_to_scalar().unwrap(), vec![]);
    assert!(matches!(scalar[0][0],Value::Float64(v) if v.is_nan()));
}

// One persisted temporal DAG accepts different request windows and detects resets.
#[test]
fn temporal_dag_uses_bound_window_without_recompilation() {
    let dag = compile_temporal(&AggIntent::Rate, false).unwrap();
    for start in [0, 60_000] {
        let labels = row(&[("__name__", "counter"), ("job", "api")], 0.)[0].clone();
        let samples = [(0, 5.), (30_000, 1.), (60_000, 7.)];
        let rows = samples
            .into_iter()
            .map(|(time, value)| {
                vec![
                    labels.clone(),
                    Value::Timestamp(start + time),
                    Value::Float64(value),
                    Value::Timestamp(start),
                    Value::Timestamp(start + 60_000),
                ]
            })
            .collect();
        let output = run_inputs(
            dag.clone(),
            vec![Batch::try_new(matrix_schema(), rows).unwrap()],
        )
        .unwrap();
        equal_rows(output, vec![row(&[("job", "api")], 7. / 60.)]);
    }
    let labels = row(&[("job", "api")], 0.)[0].clone();
    let rows = vec![
        vec![
            labels.clone(),
            Value::Timestamp(0),
            Value::Float64(1.),
            Value::Timestamp(0),
            Value::Timestamp(1000),
        ],
        vec![
            labels,
            Value::Timestamp(1000),
            Value::Float64(2.),
            Value::Timestamp(0),
            Value::Timestamp(2000),
        ],
    ];
    assert!(run_inputs(dag, vec![Batch::try_new(matrix_schema(), rows).unwrap()]).is_err());
}

// The quantile is an ordinary scalar input, and bucket labels are native computation.
#[test]
fn histogram_quantile_keeps_each_label_group() {
    let dag = compile_histogram_quantile();
    let buckets = vec![
        row(&[("job", "api"), ("le", "1")], 2.),
        row(&[("job", "api"), ("le", "2")], 4.),
        row(&[("job", "api"), ("le", "+Inf")], 4.),
    ];
    let output = run_inputs(
        dag,
        vec![
            Batch::try_new(scalar_schema(), vec![vec![Value::Float64(0.75)]]).unwrap(),
            Batch::try_new(vector_schema(), buckets).unwrap(),
        ],
    )
    .unwrap();
    equal_rows(output, vec![row(&[("job", "api")], 1.5)]);
}

#[test]
fn compiled_constant_needs_no_deployment_source() {
    let dag = compile_scalar(3.).unwrap();
    assert_eq!(dag.input_slots().count(), 0);
    let result = execute(&dag).unwrap();
    assert!(matches!(result[0][0], Value::Float64(3.)));
}

// Scalar broadcasting cannot silently create duplicate result identities when
// arithmetic or bool comparisons remove the metric name.
#[test]
fn scalar_broadcast_rejects_colliding_result_labels_after_recovery() {
    use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind};
    for left_scalar in [false, true] {
        for names in [["a", "a"], ["a", "b"]] {
            for (kind, return_bool) in [
                (BinaryOpKind::Arithmetic(ArithmeticOpKind::Add), false),
                (BinaryOpKind::Compare(CompareOpKind::Gt), true),
            ] {
                let dag = compile_binary(
                    &BinaryOperator {
                        kind,
                        vector_match: None,
                        checked_relative_division: false,
                        checked_finite_division: false,
                    },
                    return_bool,
                    left_scalar,
                    !left_scalar,
                )
                .unwrap();
                let vector = Batch::try_new(
                    vector_schema(),
                    vec![
                        row(&[("__name__", names[0]), ("job", "api")], 2.),
                        row(&[("__name__", names[1]), ("job", "api")], 3.),
                    ],
                )
                .unwrap();
                let scalar =
                    Batch::try_new(scalar_schema(), vec![vec![Value::Float64(1.)]]).unwrap();
                let result = run_inputs(
                    dag,
                    if left_scalar {
                        vec![scalar, vector]
                    } else {
                        vec![vector, scalar]
                    },
                );
                assert!(result.is_err(), "duplicate output label sets were accepted");
            }
        }
    }
    let dag = compile_binary(
        &BinaryOperator {
            kind: BinaryOpKind::Compare(CompareOpKind::Gt),
            vector_match: None,
            checked_relative_division: false,
            checked_finite_division: false,
        },
        false,
        false,
        true,
    )
    .unwrap();
    let rows = vec![
        row(&[("__name__", "a"), ("job", "api")], 2.),
        row(&[("__name__", "b"), ("job", "api")], 3.),
    ];
    equal_rows(
        run_inputs(
            dag,
            vec![
                Batch::try_new(vector_schema(), rows.clone()).unwrap(),
                Batch::try_new(scalar_schema(), vec![vec![Value::Float64(1.)]]).unwrap(),
            ],
        )
        .unwrap(),
        rows,
    );
}

// Persisted exact evaluation graphs, rather than the storage adapter, merge panes,
// finalize each population, and preserve the requested metric-name semantics.
#[test]
fn exact_state_evaluations_recover_and_finalize_panes() {
    use asap_executor::factory::create_planner_accumulator;
    use planner_types::ir::schema::*;

    use std::sync::Arc;
    for (kind, params, expected) in [
        (ExactKind::Sum, ExactParams::Sum, 12.),
        (ExactKind::Count, ExactParams::Count, 4.),
        (ExactKind::Min, ExactParams::Min, 1.),
        (ExactKind::Max, ExactParams::Max, 5.),
    ] {
        let family = FieldDataType::ExactAggregate(kind, params);
        for preserve in [false, true] {
            let rows = [[1., 2.], [4., 5.]]
                .into_iter()
                .map(|samples| {
                    let mut state = create_planner_accumulator(
                        &family,
                        &SummaryUpdate::column(ColumnRef::SampleValue),
                        &GroupingStrategy::PerSubpopulationInstance,
                    )
                    .unwrap();
                    for sample in samples {
                        state.update_single(sample, 0).unwrap();
                    }
                    let labels = row(&[("__name__", "m"), ("instance", "a")], 0.).remove(0);
                    vec![
                        labels,
                        Value::Summary {
                            family: family.clone(),
                            state: Arc::from(state.into_accumulator()),
                        },
                    ]
                })
                .collect();
            let output = run_inputs(
                compile_exact_evaluation(family.clone(), 60_000, preserve).unwrap(),
                vec![Batch::try_new(exact_state_schema(family.clone()), rows).unwrap()],
            )
            .unwrap();
            let labels = if preserve {
                vec![("__name__", "m"), ("instance", "a")]
            } else {
                vec![("instance", "a")]
            };
            equal_rows(output, vec![row(&labels, expected)]);
        }
    }
}

#[test]
fn recovered_exact_counter_uses_window_and_omits_insufficient_samples() {
    use asap_executor::factory::create_planner_accumulator;
    use planner_types::ir::schema::*;

    use std::sync::Arc;
    for (kind, params, expected) in [
        (ExactKind::Rate, ExactParams::Rate, 1.),
        (ExactKind::Increase, ExactParams::Increase, 60.),
    ] {
        let family = FieldDataType::ExactAggregate(kind, params);
        let rows = [1, 2]
            .into_iter()
            .map(|count| {
                let mut state = create_planner_accumulator(
                    &family,
                    &SummaryUpdate::column(ColumnRef::SampleValue),
                    &GroupingStrategy::PerSubpopulationInstance,
                )
                .unwrap();
                state.update_single(100., -50_000).unwrap();
                if count == 2 {
                    state.update_single(140., -10_000).unwrap();
                }
                vec![
                    row(&[("instance", if count == 1 { "one" } else { "two" })], 0.).remove(0),
                    Value::Summary {
                        family: family.clone(),
                        state: Arc::from(state.into_accumulator()),
                    },
                ]
            })
            .collect();
        let output = run_inputs(
            compile_exact_evaluation(family.clone(), 60_000, false).unwrap(),
            vec![Batch::try_new(exact_state_schema(family), rows).unwrap()],
        )
        .unwrap();
        equal_rows(output, vec![row(&[("instance", "two")], expected)]);
    }
}
