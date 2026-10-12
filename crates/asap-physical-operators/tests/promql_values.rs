//! Compile, persist and rebind dynamic-label computation without deployment lowering.
use asap_physical_operators::expressions::binary::{BinaryOpKind, BinaryOperator};

use asap_physical_operators::{
    operators::Operator,
    physical_planner::{promql_values::*, CompiledPhysicalDAG, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::pre_asap::{AggIntent, ColumnRef, GroupKeys};

use std::collections::BTreeMap;

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
fn run(dag: CompiledPhysicalDAG, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    run_inputs(dag, vec![Batch::try_new(vector_schema(), rows).unwrap()]).unwrap()
}
fn run_inputs(
    dag: CompiledPhysicalDAG,
    batches: Vec<Batch>,
) -> Result<Vec<Vec<Value>>, asap_physical_operators::Error> {
    let dag =
        serde_json::from_slice::<CompiledPhysicalDAG>(&serde_json::to_vec(&dag).unwrap()).unwrap();
    let sources = batches
        .into_iter()
        .enumerate()
        .map(|(id, batch)| {
            (
                id as u64,
                Box::new(Operator::source(batch.schema().clone(), vec![batch]).unwrap())
                    as Source<'_>,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let bound = dag.instantiate(sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = bound.execute(dag.roots(), context).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            rows.extend(batch?.rows().iter().cloned());
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
                    accuracy: planner_types::types::AccuracyTarget::Exact,
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
    let dag = compile_histogram_quantile().unwrap();
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

// Linking an ensemble preserves its shared producer and every selected operator.
#[test]
fn composed_ensemble_shares_a_producer_across_roots() {
    use asap_physical_operators::{
        physical_planner::InputContract,
        plan::{PhysicalOperator, PlanProperties},
        runtime::{Input, OutputStream},
        values::SchemaRef,
    };
    use planner_types::pre_asap::ArithmeticOpKind;
    struct Counted {
        source: Operator,
        starts: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl PhysicalOperator<Batch, SchemaRef> for Counted {
        fn name(&self) -> &str {
            "CountedInput"
        }
        fn input_schemas(&self) -> Vec<SchemaRef> {
            vec![]
        }
        fn output_schema(&self) -> SchemaRef {
            self.source.schema()
        }
        fn output_bytes(&self, batch: &Batch) -> usize {
            batch.bytes()
        }
        fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
            self.source.properties(inputs)
        }
        fn start<'a>(
            &'a self,
            inputs: Vec<Input<'a, Batch>>,
            context: RunContext,
        ) -> Result<OutputStream<'a, Batch>, asap_physical_operators::Error> {
            self.starts.set(self.starts.get() + 1);
            self.source.start(inputs, context)
        }
    }
    let aggregate = compile_aggregate(
        &AggIntent::Sum { col: None },
        &GroupKeys::by(vec![ColumnRef::Named("job".into())]),
    )
    .unwrap();
    let binary = compile_binary(
        &BinaryOperator {
            kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
            vector_match: None,
            checked_finite_division: false,
            checked_relative_division: false,
        },
        false,
        false,
        false,
    )
    .unwrap();
    let dag = CompiledPhysicalDAG::compose(
        BTreeMap::from([(0, InputContract::bounded(vector_schema()))]),
        BTreeMap::from([
            (10, (vec![0], aggregate)),
            (20, (vec![10, 10], binary)),
            (30, (vec![10], compile_negate(false).unwrap())),
        ]),
        vec![20, 30],
    )
    .unwrap();
    let dag =
        serde_json::from_slice::<CompiledPhysicalDAG>(&serde_json::to_vec(&dag).unwrap()).unwrap();
    assert_eq!(dag.input_contracts().count(), 1);
    let starts = std::rc::Rc::new(std::cell::Cell::new(0));
    for _ in 0..2 {
        let input = Batch::try_new(vector_schema(), vec![row(&[("job", "api")], 3.)]).unwrap();
        let source = Counted {
            source: Operator::source(vector_schema(), vec![input]).unwrap(),
            starts: starts.clone(),
        };
        let bound = dag
            .instantiate(BTreeMap::from([(0, Box::new(source) as Source<'_>)]))
            .unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits {
                max_buffered_batches: 1,
                ..Limits::default()
            },
        )
        .unwrap();
        let results =
            block_on(futures::future::join_all(
                bound
                    .execute(dag.roots(), context)
                    .unwrap()
                    .into_iter()
                    .map(|mut stream| async move {
                        stream.next().await.unwrap().unwrap().rows().to_vec()
                    }),
            ));
        equal_rows(results[0].clone(), vec![row(&[("job", "api")], 6.)]);
        equal_rows(results[1].clone(), vec![row(&[("job", "api")], -3.)]);
    }
    assert_eq!(starts.get(), 2);
}

#[test]
fn compiled_constant_needs_no_deployment_source() {
    let dag = compile_scalar(3.).unwrap();
    assert_eq!(dag.input_contracts().count(), 0);
    let result = run_inputs(dag, vec![]).unwrap();
    assert!(matches!(result[0][0], Value::Float64(3.)));
}

// Scalar broadcasting cannot silently create duplicate result identities when
// arithmetic or bool comparisons remove the metric name.
#[test]
fn scalar_broadcast_rejects_colliding_result_labels_after_recovery() {
    use planner_types::pre_asap::{ArithmeticOpKind, CompareOpKind};
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
    use asap_physical_operators::factory::create_planner_accumulator;
    use planner_types::post_asap::*;
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
                        state.update_single(sample, 0);
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
                vec![Batch::try_new(exact_state_schema(family.clone()).unwrap(), rows).unwrap()],
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
    use asap_physical_operators::factory::create_planner_accumulator;
    use planner_types::post_asap::*;
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
                state.update_single(100., -50_000);
                if count == 2 {
                    state.update_single(140., -10_000);
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
            vec![Batch::try_new(exact_state_schema(family).unwrap(), rows).unwrap()],
        )
        .unwrap();
        equal_rows(output, vec![row(&[("instance", "two")], expected)]);
    }
}
