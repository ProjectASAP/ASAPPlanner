//! The row-at-a-time operators this crate ran before their columnar
//! versions, kept for tests only: a test runs a DAG once with
//! [`with_row_reference`] and once without, and compares.
use super::*;
use std::cell::Cell;

mod aligned_binary;
mod current_series;
mod joins;
mod limit;
mod series_labels;
mod series_window;
mod sort;
mod vector_binary;
mod vector_window;
mod window;

thread_local! {
    /// Whether operators on this thread run their row reference.
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with the row references of converted operators.
pub(crate) fn with_row_reference<T>(f: impl FnOnce() -> T) -> T {
    ENABLED.with(|enabled| enabled.set(true));
    let result = f();
    ENABLED.with(|enabled| enabled.set(false));
    result
}

/// Whether `kind` runs its row reference now.
pub(super) fn enabled_for(kind: &Kind) -> bool {
    ENABLED.with(Cell::get)
        && matches!(
            kind,
            Kind::Limit { .. }
                | Kind::Sort { .. }
                | Kind::CurrentSeries { .. }
                | Kind::SeriesWindow { .. }
                | Kind::Window { .. }
                | Kind::RangeWindow { .. }
                | Kind::HistogramQuantile
                | Kind::VectorBinary { .. }
                | Kind::AlignedBinary { .. }
                | Kind::Join { .. }
                | Kind::SemiJoin { .. }
                | Kind::SeriesBinary { .. }
                | Kind::SeriesRelabel { .. }
                | Kind::SeriesHistogramQuantile { .. }
        )
}

/// The row reference of `operator`, when it has one and references are on.
pub(super) fn start<'a>(
    operator: &'a Operator,
    inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Option<Result<OutputStream<'a, Batch>, Error>> {
    if !ENABLED.with(Cell::get) {
        return None;
    }
    Some(match operator.kind {
        Kind::Limit { .. } => limit::execute(operator, inputs, context),
        Kind::Sort { .. } => sort::execute(operator, inputs, context),
        Kind::CurrentSeries { .. } => current_series::execute(operator, inputs, context),
        Kind::SeriesWindow { .. } => series_window::execute(operator, inputs, context),
        Kind::Window { .. } => window::execute(operator, inputs, context),
        Kind::VectorBinary { .. } => vector_binary::execute(operator, inputs, context),
        Kind::AlignedBinary { .. } => aligned_binary::execute(operator, inputs, context),
        Kind::Join { .. } | Kind::SemiJoin { .. } => joins::execute(operator, inputs, context),
        Kind::SeriesBinary { .. }
        | Kind::SeriesRelabel { .. }
        | Kind::SeriesHistogramQuantile { .. } => series_labels::execute(operator, inputs, context),
        Kind::RangeWindow { .. } | Kind::HistogramQuantile => {
            vector_window::execute(operator, inputs, context)
        }
        _ => return None,
    })
}

/// Test support: run DAGs with and without the row reference, and inputs.
pub(crate) mod harness {
    use super::*;
    use crate::plan::{NodeId, PhysicalDAG};
    use crate::runtime::{Limits, Scope};
    use planner_types::ir::schema::Field;

    /// The rows `root` of the DAG `build()` makes yields in `scope`, or its
    /// error's text.
    fn run(
        build: &dyn Fn() -> PhysicalDAG<'static, Batch, SchemaRef>,
        root: NodeId,
        scope: Scope,
    ) -> Result<Vec<Vec<Value>>, String> {
        let dag = build();
        let context = RunContext::new(scope, Limits::default()).unwrap();
        futures::executor::block_on(async {
            let mut output = dag
                .execute(&[root], context)
                .map_err(|e| e.to_string())?
                .remove(0);
            let mut rows = Vec::new();
            while let Some(batch) = output.next().await {
                rows.extend(batch.map_err(|e| e.to_string())?.to_rows().iter().cloned());
            }
            Ok(rows)
        })
    }

    /// A summary state's content: an exact state serialized, a KLL's size
    /// and quantiles, a keyed frequency state serialized.
    pub(crate) fn state_fingerprint(state: &dyn crate::AggregateCore) -> String {
        use crate::summary_kernels::{
            exact::ExactAccumulator, weighted_frequency::WeightedFrequency,
            DatasketchesKLLAccumulator,
        };
        let any = state.as_any();
        if let Some(exact) = any.downcast_ref::<ExactAccumulator>() {
            return serde_json::to_string(exact).unwrap();
        }
        if let Some(kll) = any.downcast_ref::<DatasketchesKLLAccumulator>() {
            return format!(
                "KLL k={} n={} q={:?}",
                kll.inner.k(),
                kll.inner.count(),
                [0.1, 0.5, 0.9].map(|q| kll.inner.quantile(q).to_bits())
            );
        }
        if let Some(frequency) = any.downcast_ref::<WeightedFrequency>() {
            return format!(
                "{:?} {:?}",
                frequency.counters(),
                frequency.rows(usize::MAX)
            );
        }
        panic!("no fingerprint for this state kind")
    }

    /// Exact equality of rows, float bits included.
    pub(crate) fn same_rows(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
        fn same(a: &Value, b: &Value) -> bool {
            match (a, b) {
                (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
                (Value::List(a), Value::List(b)) | (Value::Struct(a), Value::Struct(b)) => {
                    a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same(a, b))
                }
                (Value::Map(a), Value::Map(b)) => {
                    a.len() == b.len()
                        && a.iter()
                            .zip(b.iter())
                            .all(|((ak, av), (bk, bv))| same(ak, bk) && same(av, bv))
                }
                (Value::Summary { state: a, .. }, Value::Summary { state: b, .. }) => {
                    state_fingerprint(a.as_ref()) == state_fingerprint(b.as_ref())
                }
                _ => format!("{a:?}") == format!("{b:?}"),
            }
        }
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(a, b)| a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b)))
    }

    /// Assert the DAG's `root` yields the same rows (or the same error)
    /// with the columnar operators as with their row reference.
    pub(crate) fn assert_matches_reference(
        case: &str,
        build: &dyn Fn() -> PhysicalDAG<'static, Batch, SchemaRef>,
        root: NodeId,
        scope: Scope,
    ) {
        let columnar = run(build, root, scope.clone());
        let reference = with_row_reference(|| run(build, root, scope));
        match (&columnar, &reference) {
            (Ok(c), Ok(r)) => assert!(same_rows(c, r), "{case}\ncolumnar  {c:?}\nreference {r:?}"),
            (Err(c), Err(r)) => assert_eq!(c, r, "{case}"),
            _ => panic!("{case}\ncolumnar  {columnar:?}\nreference {reference:?}"),
        }
    }

    /// A query scope at `time_ms`.
    pub(crate) fn query(time_ms: i64) -> Scope {
        Scope::Query {
            evaluation_time_ms: time_ms,
        }
    }

    pub(crate) fn plain_field(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: SummaryFamilyType::Plain(dtype),
            nullable,
            table: None,
        }
    }

    /// A deterministic generator of test values.
    pub(crate) struct Generator(u64);
    impl Generator {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed)
        }
        pub(crate) fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
        /// A value of `dtype`, NULL one time in `null_every` when nullable.
        pub(crate) fn value(&mut self, dtype: &DataType, null_every: usize) -> Value {
            if null_every > 0 && self.next().is_multiple_of(null_every) {
                return Value::Null;
            }
            match dtype {
                DataType::Int64 => Value::Int64((self.next() % 5) as i64 - 2),
                DataType::Float64 => {
                    let floats = [0.0, -0.0, 1.5, f64::NAN, -2.0, 1e16, 0.1, f64::INFINITY];
                    Value::Float64(floats[self.next() % floats.len()])
                }
                DataType::Utf8 => Value::Utf8(["a", "b", "c", ""][self.next() % 4].into()),
                DataType::Bool => Value::Bool(self.next().is_multiple_of(2)),
                DataType::Timestamp => Value::Timestamp((self.next() % 6) as i64 * 1000),
                DataType::Date => Value::Date((self.next() % 3) as i32),
                _ => Value::Null,
            }
        }
        /// `batches` batches of `rows` random rows of `schema`.
        pub(crate) fn batches(
            &mut self,
            schema: &SchemaRef,
            batches: usize,
            rows: usize,
        ) -> Vec<Batch> {
            (0..batches)
                .map(|_| {
                    let rows = (0..rows)
                        .map(|_| {
                            schema
                                .fields
                                .iter()
                                .map(|field| {
                                    let SummaryFamilyType::Plain(dtype) = &field.dtype else {
                                        unreachable!()
                                    };
                                    self.value(dtype, if field.nullable { 5 } else { 0 })
                                })
                                .collect()
                        })
                        .collect();
                    Batch::try_new(schema.clone(), rows).unwrap()
                })
                .collect()
        }
    }

    /// A DAG of a source of `batches` and `operator` over it (node 1).
    pub(crate) fn over_source(
        schema: SchemaRef,
        batches: Vec<Batch>,
        operator: Operator,
    ) -> PhysicalDAG<'static, Batch, SchemaRef> {
        let mut dag = PhysicalDAG::default();
        dag.add(0, vec![], Operator::source(schema, batches).unwrap())
            .unwrap();
        dag.add(1, vec![0], operator).unwrap();
        dag
    }
}

#[cfg(test)]
mod tests {
    use super::harness::*;
    use super::*;

    fn mixed_schema() -> SchemaRef {
        Arc::new(planner_types::ir::schema::Schema {
            fields: vec![
                plain_field("text", DataType::Utf8, true),
                plain_field("int", DataType::Int64, true),
                plain_field("float", DataType::Float64, true),
                plain_field("flag", DataType::Bool, false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        })
    }

    // Sort orders groups and rows as the row operator, ties in input order.
    #[test]
    fn sort_matches_the_row_reference() {
        use crate::operators::SortKey;
        let schema = mixed_schema();
        let batches = Generator::new(7).batches(&schema, 3, 50);
        for groups in [vec![], vec![3], vec![0, 3]] {
            for keys in [
                vec![(2, false, false)],
                vec![(2, true, true), (1, false, true)],
                vec![(0, true, false), (2, false, false)],
            ] {
                let keys = keys
                    .into_iter()
                    .map(|(column, descending, nulls_first)| SortKey {
                        column,
                        descending,
                        nulls_first,
                    })
                    .collect::<Vec<_>>();
                let operator =
                    Operator::sort(schema.clone(), keys.clone(), groups.clone()).unwrap();
                let (schema, batches) = (schema.clone(), batches.clone());
                assert_matches_reference(
                    &format!("sort {keys:?} groups {groups:?}"),
                    &move || over_source(schema.clone(), batches.clone(), operator.clone()),
                    1,
                    query(0),
                );
            }
        }
    }

    /// Series samples: 0 identity, 1 time, 2 value (stale markers
    /// included), 3 a label.
    fn series_batches(
        seed: u64,
        batches: usize,
        rows: usize,
        unique_times: bool,
    ) -> (SchemaRef, Vec<Batch>) {
        let schema = Arc::new(planner_types::ir::schema::Schema {
            fields: vec![
                plain_field("$promql_series_identity", DataType::Utf8, false),
                plain_field("ts", DataType::Timestamp, false),
                plain_field("value", DataType::Float64, false),
                plain_field("job", DataType::Utf8, false),
            ],
            time_index: Some(1),
            unique_keys: vec![],
            closed: false,
        });
        let mut g = Generator::new(seed);
        let batches = (0..batches)
            .map(|b| {
                let rows = (0..rows)
                    .map(|i| {
                        let series = g.next() % 4;
                        let value = match g.next() % 9 {
                            0 => f64::from_bits(crate::operators::STALE_MARKER),
                            n => (n as f64) * 1.5,
                        };
                        vec![
                            Value::Utf8(
                                format!("{{\"__name__\":\"m\",\"s\":\"{series}\"}}").into(),
                            ),
                            Value::Timestamp(if unique_times {
                                ((b * rows + i) * 250) as i64
                            } else {
                                (g.next() % 12) as i64 * 500
                            }),
                            Value::Float64(value),
                            Value::Utf8(["api", "web"][series % 2].into()),
                        ]
                    })
                    .collect();
                Batch::try_new(schema.clone(), rows).unwrap()
            })
            .collect();
        (schema, batches)
    }

    // CurrentSeries keeps each series' latest sample (or the same error).
    #[test]
    fn current_series_matches_the_row_reference() {
        for seed in [1, 2, 3, 4] {
            let (schema, batches) = series_batches(seed, 2, 20, false);
            for lookback in [1000, 3000, 10_000] {
                let operator = Operator::current_series(schema.clone(), 0, 1, 2, lookback).unwrap();
                let (schema, batches) = (schema.clone(), batches.clone());
                assert_matches_reference(
                    &format!("current series seed {seed} lookback {lookback}"),
                    &move || over_source(schema.clone(), batches.clone(), operator.clone()),
                    1,
                    query(5000),
                );
            }
        }
    }

    // SeriesWindow evaluates each series at each instant as the row operator.
    #[test]
    fn series_window_matches_the_row_reference() {
        use planner_types::ir::operator::AggIntent;
        let functions = [
            None,
            Some(AggIntent::Rate),
            Some(AggIntent::Sum { col: None }),
            Some(AggIntent::LastOverTime),
            Some(AggIntent::Quantile {
                col: None,
                q: 0.5,
                accuracy: planner_types::ir::properties::AccuracyTarget::exact(),
            }),
        ];
        for seed in [5, 6] {
            let (schema, batches) = series_batches(seed, 2, 25, true);
            for function in &functions {
                for steps in [
                    None,
                    Some(crate::operators::SubquerySteps {
                        range_ms: 3000,
                        step_ms: 1000,
                        offset_ms: 0,
                        at_ms: None,
                    }),
                ] {
                    let operator = Operator::series_window(
                        schema.clone(),
                        function.clone(),
                        2000,
                        0,
                        None,
                        steps,
                    )
                    .unwrap();
                    let (schema, batches) = (schema.clone(), batches.clone());
                    assert_matches_reference(
                        &format!("series window {function:?} steps {steps:?} seed {seed}"),
                        &move || over_source(schema.clone(), batches.clone(), operator.clone()),
                        1,
                        query(6000),
                    );
                }
            }
        }
    }

    // The temporal Window aggregate gives the row operator's groups and values.
    #[test]
    fn window_matches_the_row_reference() {
        use planner_types::ir::operator::AggIntent;
        for seed in [8, 9] {
            let (schema, batches) = series_batches(seed, 2, 20, true);
            for intent in [
                AggIntent::Rate,
                AggIntent::Increase,
                AggIntent::Sum { col: None },
                AggIntent::Max { col: None },
                AggIntent::Count {
                    accuracy: planner_types::ir::properties::AccuracyTarget::exact(),
                },
            ] {
                for groups in [vec![], vec![0], vec![3]] {
                    for window in [(0, 10_000), (2000, 5000)] {
                        let operator = Operator::window(
                            schema.clone(),
                            intent.clone(),
                            1,
                            2,
                            groups.clone(),
                            Some(window),
                        )
                        .unwrap();
                        let (schema, batches) = (schema.clone(), batches.clone());
                        assert_matches_reference(
                            &format!("window {intent:?} groups {groups:?} {window:?}"),
                            &move || over_source(schema.clone(), batches.clone(), operator.clone()),
                            1,
                            query(10_000),
                        );
                    }
                }
            }
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (Value::Utf8((*k).into()), Value::Utf8((*v).into())))
                .collect::<Vec<_>>()
                .into(),
        )
    }

    // RangeWindow and HistogramQuantile give the row operators' series.
    #[test]
    fn vector_windows_match_the_row_reference() {
        use crate::operators::vector_window::matrix_schema;
        use crate::plan::PhysicalDAG;
        use planner_types::ir::operator::AggIntent;
        let mut g = Generator::new(11);
        let matrix_rows = (0..40)
            .map(|i| {
                let series = g.next() % 3;
                vec![
                    labels(&[("__name__", "m"), ("s", ["a", "b", "c"][series])]),
                    Value::Timestamp(i * 100),
                    Value::Float64((g.next() % 7) as f64),
                    Value::Timestamp(0),
                    Value::Timestamp(5000),
                ]
            })
            .collect::<Vec<_>>();
        let matrix = vec![
            Batch::try_new(matrix_schema(), matrix_rows[..25].to_vec()).unwrap(),
            Batch::try_new(matrix_schema(), matrix_rows[25..].to_vec()).unwrap(),
        ];
        for intent in [
            AggIntent::Rate,
            AggIntent::Sum { col: None },
            AggIntent::Count {
                accuracy: planner_types::ir::properties::AccuracyTarget::exact(),
            },
        ] {
            let operator = Operator::range_window(intent.clone()).unwrap();
            let matrix = matrix.clone();
            assert_matches_reference(
                &format!("range window {intent:?}"),
                &move || over_source(matrix_schema(), matrix.clone(), operator.clone()),
                1,
                query(5000),
            );
        }
        let value_schema = crate::operators::vector_binary::value_schema(false);
        let bucket_rows = ["0.5", "1", "+Inf", "2", "x"]
            .iter()
            .flat_map(|le| {
                ["a", "b"].map(|job| {
                    vec![
                        labels(&[("__name__", "h"), ("job", job), ("le", le)]),
                        Value::Float64(le.len() as f64 + job.len() as f64),
                    ]
                })
            })
            .collect::<Vec<_>>();
        for q in [0.5, 0.9, -1.0] {
            let buckets = Batch::try_new(value_schema.clone(), bucket_rows.clone()).unwrap();
            let scalar_schema = crate::operators::vector_binary::value_schema(true);
            let scalar =
                Batch::try_new(scalar_schema.clone(), vec![vec![Value::Float64(q)]]).unwrap();
            let value_schema = value_schema.clone();
            assert_matches_reference(
                &format!("histogram quantile {q}"),
                &move || {
                    let mut dag = PhysicalDAG::default();
                    dag.add(
                        0,
                        vec![],
                        Operator::source(scalar_schema.clone(), vec![scalar.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(
                        1,
                        vec![],
                        Operator::source(value_schema.clone(), vec![buckets.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(2, vec![0, 1], Operator::histogram_quantile())
                        .unwrap();
                    dag
                },
                2,
                query(0),
            );
        }
    }

    // VectorBinary matches and computes as the row operator.
    #[test]
    fn vector_binary_matches_the_row_reference() {
        use crate::expressions::binary::{BinaryOpKind, BinaryOperator};
        use crate::operators::vector_binary::value_schema;
        use crate::plan::PhysicalDAG;
        use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind};
        let vector = |values: &[(&str, &str, f64)]| {
            Batch::try_new(
                value_schema(false),
                values
                    .iter()
                    .map(|(name, s, v)| {
                        vec![labels(&[("__name__", name), ("s", s)]), Value::Float64(*v)]
                    })
                    .collect(),
            )
            .unwrap()
        };
        let scalar =
            |v: f64| Batch::try_new(value_schema(true), vec![vec![Value::Float64(v)]]).unwrap();
        let left = vector(&[("a", "1", 1.0), ("a", "2", 5.0), ("a", "3", f64::NAN)]);
        let right = vector(&[("b", "1", 2.0), ("b", "3", 3.0), ("b", "4", 4.0)]);
        let duplicate = vector(&[("a", "1", 1.0), ("b", "1", 5.0)]);
        let operator = |kind| BinaryOperator {
            kind,
            vector_match: None,
            checked_relative_division: false,
            checked_finite_division: false,
        };
        for (kind, return_bool) in [
            (BinaryOpKind::Arithmetic(ArithmeticOpKind::Add), false),
            (BinaryOpKind::Arithmetic(ArithmeticOpKind::Div), false),
            (BinaryOpKind::Compare(CompareOpKind::Lt), false),
            (BinaryOpKind::Compare(CompareOpKind::Lt), true),
        ] {
            for (l, r, ls, rs) in [
                (left.clone(), right.clone(), false, false),
                (scalar(2.0), right.clone(), true, false),
                (left.clone(), scalar(3.0), false, true),
                (duplicate.clone(), right.clone(), false, false),
                (scalar(2.0), scalar(3.0), true, true),
            ] {
                let Ok(binary) = Operator::vector_binary(
                    value_schema(ls),
                    value_schema(rs),
                    operator(kind.clone()),
                    return_bool,
                ) else {
                    continue;
                };
                let (l, r) = (l.clone(), r.clone());
                assert_matches_reference(
                    &format!("vector binary {kind:?} bool {return_bool} scalars {ls} {rs}"),
                    &move || {
                        let mut dag = PhysicalDAG::default();
                        dag.add(
                            0,
                            vec![],
                            Operator::source(value_schema(ls), vec![l.clone()]).unwrap(),
                        )
                        .unwrap();
                        dag.add(
                            1,
                            vec![],
                            Operator::source(value_schema(rs), vec![r.clone()]).unwrap(),
                        )
                        .unwrap();
                        dag.add(2, vec![0, 1], binary.clone()).unwrap();
                        dag
                    },
                    2,
                    query(0),
                );
            }
        }
    }

    // Joins pair the same rows as the row joins, outer rows and errors
    // included; semi-joins and aligned arithmetic match the same keys.
    #[test]
    fn joins_match_the_row_reference() {
        use crate::plan::PhysicalDAG;
        use planner_types::ir::operator::JoinKind;
        use planner_types::ir::scalar::{
            ArithmeticOpKind, CompareOpKind, ExprSemantics, ScalarValue,
        };
        use planner_types::ir::{Predicate, ScalarExpr};
        let schema = mixed_schema();
        let mut g = Generator::new(13);
        let left = g.batches(&schema, 2, 12);
        let right = g.batches(&schema, 1, 15);
        // Huge integers on the right make `left.int + right.int` overflow.
        let mut huge_rows = right[0].to_rows().to_vec();
        huge_rows[4][1] = Value::Int64(i64::MAX);
        let huge = vec![Batch::try_new(schema.clone(), huge_rows).unwrap()];
        let compare = |op, l: ScalarExpr, r: ScalarExpr| {
            Predicate(ScalarExpr::Compare {
                left: Box::new(l),
                op,
                right: Box::new(r),
                semantics: ExprSemantics::Sql,
            })
        };
        let predicates = [
            compare(
                CompareOpKind::Eq,
                ScalarExpr::Column(1),
                ScalarExpr::Column(5),
            ),
            compare(
                CompareOpKind::Lt,
                ScalarExpr::Column(2),
                ScalarExpr::Column(6),
            ),
            compare(
                CompareOpKind::Gt,
                ScalarExpr::Arithmetic {
                    op: ArithmeticOpKind::Add,
                    left: Box::new(ScalarExpr::Column(1)),
                    right: Box::new(ScalarExpr::Column(5)),
                    semantics: ExprSemantics::Sql,
                },
                ScalarExpr::Literal(ScalarValue::Int64(0)),
            ),
        ];
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Right,
            JoinKind::Full,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::Cross,
        ] {
            for (index, predicate) in predicates.iter().enumerate() {
                for right_batches in [right.clone(), huge.clone()] {
                    let mut joined = schema.fields.clone();
                    joined.extend(schema.fields.iter().cloned());
                    let output = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                        schema.clone()
                    } else {
                        let mut fields = joined;
                        for field in &mut fields {
                            field.nullable = true;
                        }
                        Arc::new(planner_types::ir::schema::Schema {
                            fields,
                            time_index: None,
                            unique_keys: vec![],
                            closed: false,
                        })
                    };
                    let Ok(join) = Operator::relational_join(
                        schema.clone(),
                        schema.clone(),
                        kind.clone(),
                        predicate,
                        output,
                    ) else {
                        continue;
                    };
                    let (schema, left) = (schema.clone(), left.clone());
                    assert_matches_reference(
                        &format!("join {kind:?} predicate {index}"),
                        &move || {
                            let mut dag = PhysicalDAG::default();
                            dag.add(
                                0,
                                vec![],
                                Operator::source(schema.clone(), left.clone()).unwrap(),
                            )
                            .unwrap();
                            dag.add(
                                1,
                                vec![],
                                Operator::source(schema.clone(), right_batches.clone()).unwrap(),
                            )
                            .unwrap();
                            dag.add(2, vec![0, 1], join.clone()).unwrap();
                            dag
                        },
                        2,
                        query(0),
                    );
                }
            }
        }
        for keys in [vec![(0, 0)], vec![(1, 1), (3, 3)], vec![(2, 2)]] {
            for certified in [false, true] {
                let mut semi =
                    Operator::semi_join(schema.clone(), schema.clone(), keys.clone()).unwrap();
                if certified {
                    semi = semi.require_complete_right();
                }
                let (schema, left, right) = (schema.clone(), left.clone(), right.clone());
                assert_matches_reference(
                    &format!("semi join {keys:?} certified {certified}"),
                    &move || {
                        let mut dag = PhysicalDAG::default();
                        dag.add(
                            0,
                            vec![],
                            Operator::source(schema.clone(), left.clone()).unwrap(),
                        )
                        .unwrap();
                        dag.add(
                            1,
                            vec![],
                            Operator::source(schema.clone(), right.clone()).unwrap(),
                        )
                        .unwrap();
                        dag.add(2, vec![0, 1], semi.clone()).unwrap();
                        dag
                    },
                    2,
                    query(0),
                );
            }
        }
    }

    // Aligned arithmetic matches each key once, as the row operator.
    #[test]
    fn aligned_binary_matches_the_row_reference() {
        use crate::expressions::binary::{BinaryOpKind, BinaryOperator};
        use crate::plan::PhysicalDAG;
        use planner_types::ir::scalar::ArithmeticOpKind;
        let schema = Arc::new(planner_types::ir::schema::Schema {
            fields: vec![
                plain_field("k", DataType::Utf8, false),
                plain_field("j", DataType::Int64, false),
                plain_field("value", DataType::Float64, false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let rows = |keys: &[(&str, i64, f64)]| {
            Batch::try_new(
                schema.clone(),
                keys.iter()
                    .map(|(k, j, v)| {
                        vec![
                            Value::Utf8((*k).into()),
                            Value::Int64(*j),
                            Value::Float64(*v),
                        ]
                    })
                    .collect(),
            )
            .unwrap()
        };
        let left = rows(&[("a", 1, 1.0), ("b", 1, 2.0), ("a", 2, 3.0)]);
        let cases = [
            rows(&[("a", 2, 10.0), ("a", 1, 20.0), ("b", 1, 30.0)]),
            rows(&[("a", 2, 10.0), ("a", 2, 20.0), ("b", 1, 30.0)]),
            rows(&[("a", 2, 10.0), ("c", 1, 20.0), ("b", 1, 30.0)]),
            rows(&[("a", 2, f64::INFINITY), ("a", 1, 20.0), ("b", 1, 30.0)]),
        ];
        for (index, right) in cases.into_iter().enumerate() {
            let operator = Operator::aligned_binary(
                schema.clone(),
                schema.clone(),
                vec![(0, 0), (1, 1)],
                (2, 2),
                BinaryOperator {
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                    checked_relative_division: false,
                    checked_finite_division: false,
                },
            )
            .unwrap();
            let (schema, left) = (schema.clone(), left.clone());
            assert_matches_reference(
                &format!("aligned case {index}"),
                &move || {
                    let mut dag = PhysicalDAG::default();
                    dag.add(
                        0,
                        vec![],
                        Operator::source(schema.clone(), vec![left.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(
                        1,
                        vec![],
                        Operator::source(schema.clone(), vec![right.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(2, vec![0, 1], operator.clone()).unwrap();
                    dag
                },
                2,
                query(0),
            );
        }
    }

    /// One sample per series `names[i]` of metric `metric`, at time 1000.
    fn instant_vector(metric: &str, series: &[(&str, &str, f64)]) -> (SchemaRef, Batch) {
        let (schema, _) = series_batches(0, 0, 0, true);
        let rows = series
            .iter()
            .map(|(s, job, value)| {
                vec![
                    Value::Utf8(
                        format!("{{\"__name__\":\"{metric}\",\"job\":\"{job}\",\"s\":\"{s}\"}}")
                            .into(),
                    ),
                    Value::Timestamp(1000),
                    Value::Float64(*value),
                    Value::Utf8((*job).into()),
                ]
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        (schema, batch)
    }

    // The PromQL series operators (binary, relabel, histogram quantile)
    // give the row operators' series, values and errors.
    #[test]
    fn series_operators_match_the_row_reference() {
        use crate::expressions::binary::{BinaryOpKind, BinaryOperator};
        use crate::plan::PhysicalDAG;
        use planner_types::ir::operator::{
            GroupSide, PromQLVectorSetOpKind, VectorGrouping, VectorMatch, VectorMatchKind,
        };
        use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind};
        let (schema, left) = instant_vector(
            "m",
            &[("1", "api", 1.0), ("2", "web", 5.0), ("3", "api", 2.0)],
        );
        let (_, right) = instant_vector(
            "n",
            &[("1", "api", 2.0), ("3", "api", 3.0), ("4", "db", 4.0)],
        );
        let (_, by_job) = instant_vector("j", &[("x", "api", 10.0), ("y", "web", 20.0)]);
        let scalar_schema = crate::operators::vector_binary::value_schema(true);
        let scalar_schema = Arc::new(planner_types::ir::schema::Schema {
            fields: vec![plain_field("value", DataType::Float64, false)],
            ..(*scalar_schema).clone()
        });
        let scalar =
            Batch::try_new(scalar_schema.clone(), vec![vec![Value::Float64(2.0)]]).unwrap();
        let operator = |kind, vector_match| BinaryOperator {
            kind,
            vector_match,
            checked_relative_division: false,
            checked_finite_division: false,
        };
        let on_job = |side: Option<GroupSide>| {
            Some(VectorMatch {
                kind: VectorMatchKind::On,
                labels: vec!["job".into()],
                grouping: side.map(|side| VectorGrouping {
                    side,
                    labels: vec!["s".into()],
                }),
            })
        };
        let cases: Vec<(BinaryOperator, [bool; 2], Batch, Batch)> = vec![
            (
                operator(BinaryOpKind::Arithmetic(ArithmeticOpKind::Add), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(BinaryOpKind::Compare(CompareOpKind::Lt), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(BinaryOpKind::CompareBool(CompareOpKind::Lt), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(BinaryOpKind::Set(PromQLVectorSetOpKind::And), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(BinaryOpKind::Set(PromQLVectorSetOpKind::Or), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(BinaryOpKind::Set(PromQLVectorSetOpKind::Unless), None),
                [false, false],
                left.clone(),
                right.clone(),
            ),
            (
                operator(
                    BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
                    on_job(Some(GroupSide::Left)),
                ),
                [false, false],
                left.clone(),
                by_job.clone(),
            ),
            (
                operator(
                    BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
                    on_job(Some(GroupSide::Right)),
                ),
                [false, false],
                by_job.clone(),
                left.clone(),
            ),
            (
                operator(
                    BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
                    on_job(None),
                ),
                [false, false],
                left.clone(),
                by_job.clone(),
            ),
            (
                operator(BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub), None),
                [true, false],
                scalar.clone(),
                left.clone(),
            ),
            (
                operator(BinaryOpKind::Compare(CompareOpKind::Gt), None),
                [false, true],
                left.clone(),
                scalar.clone(),
            ),
            (
                operator(BinaryOpKind::CompareBool(CompareOpKind::Gt), None),
                [true, true],
                scalar.clone(),
                scalar.clone(),
            ),
            (
                operator(BinaryOpKind::Arithmetic(ArithmeticOpKind::Div), None),
                [false, false],
                left.clone(),
                left.clone(),
            ),
        ];
        for (index, (binary, scalars, l, r)) in cases.into_iter().enumerate() {
            let schemas = scalars.map(|scalar| {
                if scalar {
                    scalar_schema.clone()
                } else {
                    schema.clone()
                }
            });
            let Ok(series) = Operator::series_binary(
                schemas[0].clone(),
                schemas[1].clone(),
                binary.clone(),
                scalars,
            ) else {
                panic!("case {index} builds");
            };
            assert_matches_reference(
                &format!("series binary case {index} {binary:?} {scalars:?}"),
                &move || {
                    let mut dag = PhysicalDAG::default();
                    dag.add(
                        0,
                        vec![],
                        Operator::source(schemas[0].clone(), vec![l.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(
                        1,
                        vec![],
                        Operator::source(schemas[1].clone(), vec![r.clone()]).unwrap(),
                    )
                    .unwrap();
                    dag.add(2, vec![0, 1], series.clone()).unwrap();
                    dag
                },
                2,
                query(1000),
            );
        }
        // Relabel: set `job` from `s`, or to a constant.
        for (replacement, source) in [
            ("x-$1", Some(("s".to_string(), "(.*)".to_string()))),
            ("", Some(("s".to_string(), "1".to_string()))),
            ("same", None),
        ] {
            let relabel = Operator::series_relabel(
                schema.clone(),
                schema.clone(),
                "job".into(),
                replacement.into(),
                source,
            )
            .unwrap();
            let (schema, left) = (schema.clone(), left.clone());
            assert_matches_reference(
                &format!("relabel {replacement}"),
                &move || over_source(schema.clone(), vec![left.clone()], relabel.clone()),
                1,
                query(1000),
            );
        }
        // Histogram quantile over `le` buckets in the label columns.
        let bucket_schema = Arc::new(planner_types::ir::schema::Schema {
            fields: vec![
                plain_field("job", DataType::Utf8, false),
                plain_field("le", DataType::Utf8, false),
                plain_field("value", DataType::Float64, false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let buckets = Batch::try_new(
            bucket_schema.clone(),
            ["0.5", "1", "+Inf", "x"]
                .iter()
                .flat_map(|le| {
                    ["api", "web"].map(|job| {
                        vec![
                            Value::Utf8(job.into()),
                            Value::Utf8((*le).into()),
                            Value::Float64(le.len() as f64 + job.len() as f64),
                        ]
                    })
                })
                .collect(),
        )
        .unwrap();
        for q in [0.5, 0.99] {
            let histogram =
                Operator::series_histogram_quantile(bucket_schema.clone(), q, 1).unwrap();
            let (bucket_schema, buckets) = (bucket_schema.clone(), buckets.clone());
            assert_matches_reference(
                &format!("series histogram quantile {q}"),
                &move || {
                    over_source(
                        bucket_schema.clone(),
                        vec![buckets.clone()],
                        histogram.clone(),
                    )
                },
                1,
                query(1000),
            );
        }
    }

    /// A sum state over `values`.
    fn sum_state(values: &[f64]) -> Value {
        use crate::summary_kernels::exact::ExactAccumulator;
        let family = SummaryFamilyType::ExactAggregate(
            planner_types::ir::schema::ExactKind::Sum,
            planner_types::ir::schema::ExactParams::Sum,
        );
        let mut state = ExactAccumulator::new(family.clone(), false).unwrap();
        for value in values {
            state.update(None, *value, 0);
        }
        Value::Summary {
            family,
            state: Arc::new(state),
        }
    }

    // Empty inputs (no batch, or one empty batch) give the row operators'
    // results; summary state columns pass through collection, concatenation,
    // Sort, Limit and joins as the row operators passed them.
    #[test]
    fn empty_inputs_and_state_columns_match_the_row_reference() {
        use crate::operators::SortKey;
        use crate::plan::PhysicalDAG;
        use planner_types::ir::operator::{AggIntent, JoinKind};
        use planner_types::ir::scalar::{CompareOpKind, ExprSemantics};
        use planner_types::ir::{Predicate, ScalarExpr};
        let schema = mixed_schema();
        let (series_schema, _) = series_batches(0, 0, 0, true);
        let empties = |schema: &SchemaRef| {
            vec![
                vec![],
                vec![Batch::try_new(schema.clone(), vec![]).unwrap()],
            ]
        };
        let sort = Operator::sort(
            schema.clone(),
            vec![SortKey {
                column: 2,
                descending: false,
                nulls_first: false,
            }],
            vec![0],
        )
        .unwrap();
        let limit = Operator::limit(schema.clone(), 2, 0, vec![0]).unwrap();
        let current = Operator::current_series(series_schema.clone(), 0, 1, 2, 1000).unwrap();
        let window = Operator::series_window(
            series_schema.clone(),
            Some(AggIntent::Rate),
            2000,
            0,
            None,
            None,
        )
        .unwrap();
        let temporal = Operator::window(
            series_schema.clone(),
            AggIntent::Sum { col: None },
            1,
            2,
            vec![3],
            Some((0, 5000)),
        )
        .unwrap();
        for (name, input, operator) in [
            ("sort", schema.clone(), sort),
            ("limit", schema.clone(), limit),
            ("current series", series_schema.clone(), current),
            ("series window", series_schema.clone(), window),
            ("window", series_schema.clone(), temporal),
        ] {
            for batches in empties(&input) {
                let (input, operator) = (input.clone(), operator.clone());
                let count = batches.len();
                assert_matches_reference(
                    &format!("{name} over {count} empty batches"),
                    &move || over_source(input.clone(), batches.clone(), operator.clone()),
                    1,
                    query(5000),
                );
            }
        }
        let predicate = Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(1)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Column(5)),
            semantics: ExprSemantics::Sql,
        });
        let join_output = |kind: &JoinKind, left: &SchemaRef, right: &SchemaRef| {
            if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                left.clone()
            } else {
                let mut fields = left.fields.clone();
                fields.extend(right.fields.iter().cloned());
                for field in &mut fields {
                    field.nullable = true;
                }
                Arc::new(planner_types::ir::schema::Schema {
                    fields,
                    time_index: None,
                    unique_keys: vec![],
                    closed: false,
                })
            }
        };
        let some = Generator::new(21).batches(&schema, 1, 6);
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Right,
            JoinKind::Full,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::Cross,
        ] {
            for (left, right) in [
                (vec![], some.clone()),
                (some.clone(), vec![]),
                (empties(&schema)[1].clone(), some.clone()),
                (some.clone(), empties(&schema)[1].clone()),
            ] {
                let join = Operator::relational_join(
                    schema.clone(),
                    schema.clone(),
                    kind.clone(),
                    &predicate,
                    join_output(&kind, &schema, &schema),
                )
                .unwrap();
                let schema = schema.clone();
                let sizes = (left.len(), right.len());
                assert_matches_reference(
                    &format!("join {kind:?} over empty side {sizes:?}"),
                    &move || {
                        let mut dag = PhysicalDAG::default();
                        dag.add(
                            0,
                            vec![],
                            Operator::source(schema.clone(), left.clone()).unwrap(),
                        )
                        .unwrap();
                        dag.add(
                            1,
                            vec![],
                            Operator::source(schema.clone(), right.clone()).unwrap(),
                        )
                        .unwrap();
                        dag.add(2, vec![0, 1], join.clone()).unwrap();
                        dag
                    },
                    2,
                    query(0),
                );
            }
        }
        // A schema with a summary column; two batches with their own states.
        let state_schema = Arc::new(planner_types::ir::schema::Schema {
            fields: vec![
                plain_field("k", DataType::Utf8, false),
                plain_field("x", DataType::Int64, false),
                planner_types::ir::schema::Field {
                    name: "state".into(),
                    dtype: SummaryFamilyType::ExactAggregate(
                        planner_types::ir::schema::ExactKind::Sum,
                        planner_types::ir::schema::ExactParams::Sum,
                    ),
                    nullable: false,
                    table: None,
                },
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let state_batches = (0..2)
            .map(|b| {
                Batch::try_new(
                    state_schema.clone(),
                    (0..5)
                        .map(|i| {
                            vec![
                                Value::Utf8(["a", "b"][i % 2].into()),
                                Value::Int64((5 - i) as i64),
                                sum_state(&[(b * 10 + i) as f64, 0.5]),
                            ]
                        })
                        .collect(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let state_sort = Operator::sort(
            state_schema.clone(),
            vec![SortKey {
                column: 1,
                descending: true,
                nulls_first: false,
            }],
            vec![0],
        )
        .unwrap();
        let state_limit = Operator::limit(state_schema.clone(), 2, 1, vec![0]).unwrap();
        for (name, operator) in [("sort", state_sort), ("limit", state_limit)] {
            let (state_schema, state_batches) = (state_schema.clone(), state_batches.clone());
            assert_matches_reference(
                &format!("{name} with states"),
                &move || {
                    over_source(
                        state_schema.clone(),
                        state_batches.clone(),
                        operator.clone(),
                    )
                },
                1,
                query(0),
            );
        }
        let state_predicate = Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(1)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Column(4)),
            semantics: ExprSemantics::Sql,
        });
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
        ] {
            let join = Operator::relational_join(
                state_schema.clone(),
                state_schema.clone(),
                kind.clone(),
                &state_predicate,
                join_output(&kind, &state_schema, &state_schema),
            );
            let Ok(join) = join else { continue };
            let (state_schema, state_batches) = (state_schema.clone(), state_batches.clone());
            assert_matches_reference(
                &format!("join {kind:?} with states"),
                &move || {
                    let mut dag = PhysicalDAG::default();
                    dag.add(
                        0,
                        vec![],
                        Operator::source(state_schema.clone(), state_batches.clone()).unwrap(),
                    )
                    .unwrap();
                    dag.add(
                        1,
                        vec![],
                        Operator::source(state_schema.clone(), state_batches[..1].to_vec())
                            .unwrap(),
                    )
                    .unwrap();
                    dag.add(2, vec![0, 1], join.clone()).unwrap();
                    dag
                },
                2,
                query(0),
            );
        }
    }

    // Limit keeps the same rows per group as the row operator.
    #[test]
    fn limit_matches_the_row_reference() {
        let schema = mixed_schema();
        let batches = Generator::new(3).batches(&schema, 4, 40);
        for groups in [vec![], vec![0], vec![2, 3]] {
            for (n, offset) in [(0, 0), (3, 0), (2, 5), (u64::MAX, 1), (100, 0)] {
                let operator = Operator::limit(schema.clone(), n, offset, groups.clone()).unwrap();
                let (schema, batches) = (schema.clone(), batches.clone());
                assert_matches_reference(
                    &format!("limit {n} offset {offset} groups {groups:?}"),
                    &move || over_source(schema.clone(), batches.clone(), operator.clone()),
                    1,
                    query(0),
                );
            }
        }
    }
}
