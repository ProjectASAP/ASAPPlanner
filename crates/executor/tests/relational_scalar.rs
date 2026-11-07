//! Set operations, SQL window functions, PromQL series sampling and
//! subquery reads, each against the values its SQL or PromQL definition
//! gives, edge cases included (NULL, NaN, empty input, ties).
use asap_executor::{
    expressions::CompiledExpression,
    operators::{Expression, Operator, SortKey, SubqueryRead, WindowSpec},
    plan::PhysicalDAG,
    runtime::{Limits, RunContext, Scope},
    values::{Batch, SchemaRef, Value},
    Error,
};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{
    SampleKind, WindowFrame, WindowFrameBound, WindowFrameOffset, WindowFrameUnits, WindowFuncKind,
};
use planner_types::ir::scalar::ScalarValue;
use planner_types::ir::schema::{DataType, Field, Schema};
use std::sync::Arc;

fn schema(fields: &[(&str, DataType)]) -> SchemaRef {
    Arc::new(Schema::new(
        fields
            .iter()
            .map(|(name, dtype)| Field::plain(*name, dtype.clone(), true))
            .collect(),
    ))
}

/// Run `operator` over sources holding `inputs` (each its schema and
/// rows): the output rows, or the run's error.
fn run(
    inputs: Vec<(SchemaRef, Vec<Vec<Value>>)>,
    operator: Operator,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut dag = PhysicalDAG::default();
    let count = inputs.len() as u64;
    for (id, (schema, rows)) in inputs.into_iter().enumerate() {
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        dag.add(
            id as u64,
            vec![],
            Operator::source(schema, vec![batch]).unwrap(),
        )
        .unwrap();
    }
    dag.add(count, (0..count).collect(), operator).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits::default(),
    )
    .unwrap();
    let rows = block_on(async {
        let mut stream = dag.execute(&[count], context.clone())?.remove(0);
        let mut rows = vec![];
        while let Some(batch) = stream.next().await {
            rows.extend(batch?.to_rows());
        }
        Ok(rows)
    });
    assert_eq!(context.retained_bytes(), 0);
    rows
}

fn f(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Float64)
}
fn i(value: i64) -> Value {
    Value::Int64(value)
}
fn debug(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}

// INTERSECT / EXCEPT compare whole rows, NULL equal to NULL: left
// {1 ×3, 2, NULL ×2, 3}, right {1 ×2, NULL, 4}. INTERSECT keeps {1, NULL};
// INTERSECT ALL min(l, r) copies: 1, 1, NULL; EXCEPT keeps {2, 3}; EXCEPT
// ALL max(l - r, 0) copies: 1, NULL, 2, 3.
#[test]
fn intersect_and_except_follow_sql() {
    let input = schema(&[("x", DataType::Float64)]);
    let rows = |values: &[Option<f64>]| values.iter().map(|v| vec![f(*v)]).collect::<Vec<_>>();
    let left = rows(&[Some(1.), Some(1.), Some(1.), Some(2.), None, None, Some(3.)]);
    let right = rows(&[Some(1.), None, Some(4.), Some(1.)]);
    for (except, all, expected) in [
        (false, false, rows(&[Some(1.), None])),
        (false, true, rows(&[Some(1.), Some(1.), None])),
        (true, false, rows(&[Some(2.), Some(3.)])),
        (true, true, rows(&[Some(1.), Some(2.), None, Some(3.)])),
    ] {
        let operator = Operator::set_operation(input.clone(), input.clone(), except, all).unwrap();
        let actual = run(
            vec![
                (input.clone(), left.clone()),
                (input.clone(), right.clone()),
            ],
            operator,
        )
        .unwrap();
        assert_eq!(
            debug(&actual),
            debug(&expected),
            "except {except}, all {all}"
        );
    }
    // Over an empty right input, INTERSECT is empty and EXCEPT distinct.
    let empty = |except| {
        run(
            vec![(input.clone(), left.clone()), (input.clone(), vec![])],
            Operator::set_operation(input.clone(), input.clone(), except, false).unwrap(),
        )
        .unwrap()
    };
    assert!(empty(false).is_empty());
    assert_eq!(empty(true).len(), 4);
}

/// `function` (over column `v`) per row of one partition ordered by `o`,
/// with `frame`: the window column, in input order.
fn window(
    function: WindowFuncKind,
    argument: bool,
    descending: bool,
    frame: Option<WindowFrame>,
    rows: &[(Option<f64>, Option<f64>)],
) -> Vec<String> {
    window_with(function, argument, descending, frame, rows, 1, Value::Null)
}

fn window_with(
    function: WindowFuncKind,
    argument: bool,
    descending: bool,
    frame: Option<WindowFrame>,
    rows: &[(Option<f64>, Option<f64>)],
    offset: i64,
    default: Value,
) -> Vec<String> {
    let input = schema(&[("o", DataType::Float64), ("v", DataType::Float64)]);
    let operator = Operator::sql_window(
        input.clone(),
        WindowSpec {
            function,
            argument: argument.then_some(1),
            offset,
            default,
            partition: vec![],
            order: vec![SortKey {
                column: 0,
                descending,
                nulls_first: false,
            }],
            frame,
        },
        "w",
    )
    .unwrap();
    let rows = rows.iter().map(|(o, v)| vec![f(*o), f(*v)]).collect();
    run(vec![(input, rows)], operator)
        .unwrap()
        .into_iter()
        .map(|mut row| format!("{:?}", row.pop().unwrap()))
        .collect()
}

fn frame(units: WindowFrameUnits, start: WindowFrameBound, end: WindowFrameBound) -> WindowFrame {
    WindowFrame {
        units,
        start_bound: start,
        end_bound: end,
    }
}
fn preceding(n: i64) -> WindowFrameBound {
    WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Int64(n)))
}
fn following(n: i64) -> WindowFrameBound {
    WindowFrameBound::Following(WindowFrameOffset::Scalar(ScalarValue::Int64(n)))
}
fn unbounded_following() -> WindowFrameBound {
    WindowFrameBound::Following(WindowFrameOffset::Scalar(ScalarValue::Null))
}

// Frames: `ROWS 1 PRECEDING AND 1 FOLLOWING` sums neighbours; `RANGE 1
// PRECEDING AND CURRENT ROW` sums the rows whose order key is within 1
// below (above for DESC), peers included; SUM skips NULL and is NULL over
// no value; COUNT(v) counts values, COUNT(*) rows.
#[test]
fn window_frames_follow_sql() {
    use WindowFrameUnits::{Range, Rows};
    use WindowFuncKind as F;
    // Order keys 1, 2, 2, 4, NULL (last); values 10, NULL, 30, 40, 50.
    let rows = [
        (Some(1.), Some(10.)),
        (Some(2.), None),
        (Some(2.), Some(30.)),
        (Some(4.), Some(40.)),
        (None, Some(50.)),
    ];
    let around = frame(Rows, preceding(1), following(1));
    assert_eq!(
        window(F::Sum, true, false, Some(around.clone()), &rows),
        [Some(10.), Some(40.), Some(70.), Some(120.), Some(90.)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    let near = frame(Range, preceding(1), WindowFrameBound::CurrentRow);
    assert_eq!(
        window(F::Sum, true, false, Some(near.clone()), &rows),
        // 1: {1}; 2: {1, 2, 2}; 4: {4} (3 is absent); NULL: its peers.
        [Some(10.), Some(40.), Some(40.), Some(40.), Some(50.)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::Sum, true, true, Some(near), &rows),
        // Descending: within 1 above. 1: {2, 2, 1}; 2: {2, 2}; 4: {4}.
        [Some(40.), Some(30.), Some(30.), Some(40.), Some(50.)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::Count, true, false, Some(around.clone()), &rows),
        [1, 2, 2, 3, 2].map(i).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::Count, false, false, Some(around), &rows),
        [2, 3, 3, 3, 2].map(i).map(|v| format!("{v:?}"))
    );
    // The default frame with an order: up to the current row's last peer.
    assert_eq!(
        window(F::Avg, true, false, None, &rows),
        [Some(10.), Some(20.), Some(20.), Some(80. / 3.), Some(32.5)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    // A frame with no value: NULL.
    let only_second = [(Some(1.), None), (Some(2.), Some(5.))];
    assert_eq!(
        window(
            F::Max,
            true,
            false,
            Some(frame(Rows, preceding(0), following(0))),
            &only_second
        ),
        [None, Some(5.)].map(f).map(|v| format!("{v:?}"))
    );
}

// Navigation: FIRST_VALUE / LAST_VALUE / NTH_VALUE read the frame; LAG and
// LEAD reach outside it, with their default past the partition;
// leadInFrame stays in the frame, so over the default frame (up to the
// current row) it gives its default.
#[test]
fn window_navigation_follows_sql() {
    use WindowFuncKind as F;
    let rows = [
        (Some(3.), Some(30.)),
        (Some(1.), Some(10.)),
        (Some(2.), None),
    ];
    let whole = frame(
        WindowFrameUnits::Rows,
        WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Null)),
        unbounded_following(),
    );
    assert_eq!(
        window(F::FirstValue, true, false, Some(whole.clone()), &rows),
        [Some(10.); 3].map(f).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::LastValue, true, false, None, &rows),
        [Some(30.), Some(10.), None]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    // Up to the current row, the second row exists from the second on.
    let no_null = [
        (Some(3.), Some(30.)),
        (Some(1.), Some(10.)),
        (Some(2.), Some(20.)),
    ];
    assert_eq!(
        window(F::NthValue(Some(2)), true, false, None, &no_null),
        [Some(20.), None, Some(20.)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::NthValue(Some(3)), true, false, Some(whole), &rows),
        [Some(30.); 3].map(f).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window_with(F::Lag, true, false, None, &rows, 1, Value::Float64(-1.)),
        [None, Some(-1.), Some(10.)]
            .map(f)
            .map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window_with(F::Lead, true, false, None, &rows, 2, Value::Null),
        [None, Some(30.), None].map(f).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window_with(
            F::LeadInFrame,
            true,
            false,
            None,
            &rows,
            1,
            Value::Float64(0.)
        ),
        [Some(0.); 3].map(f).map(|v| format!("{v:?}"))
    );
}

// Rankings over ties: ROW_NUMBER counts, RANK skips after ties, DENSE_RANK
// does not; NULL order keys are peers, last.
#[test]
fn window_rankings_follow_sql() {
    use WindowFuncKind as F;
    let rows = [
        (Some(2.), None),
        (None, None),
        (Some(1.), None),
        (Some(2.), None),
        (None, None),
    ];
    assert_eq!(
        window(F::RowNumber, false, false, None, &rows),
        [2, 4, 1, 3, 5].map(i).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::Rank, false, false, None, &rows),
        [2, 4, 1, 2, 4].map(i).map(|v| format!("{v:?}"))
    );
    assert_eq!(
        window(F::DenseRank, false, false, None, &rows),
        [2, 3, 1, 2, 3].map(i).map(|v| format!("{v:?}"))
    );
}

/// `read` of a one-column subquery holding `values`, bound to each row of
/// a one-row input: the bound value.
fn read(read: SubqueryRead, values: &[Option<f64>]) -> Result<Value, Error> {
    let input = schema(&[("x", DataType::Float64)]);
    let subquery = schema(&[("value", DataType::Float64)]);
    let operator = Operator::bind_subquery(input.clone(), subquery.clone(), read, "bound").unwrap();
    let rows = run(
        vec![
            (input, vec![vec![f(Some(1.))]]),
            (subquery, values.iter().map(|v| vec![f(*v)]).collect()),
        ],
        operator,
    )?;
    Ok(rows[0][1].clone())
}

// A scalar subquery is NULL without a row and an error with more than one;
// EXISTS is whether it has a row.
#[test]
fn scalar_subqueries_follow_sql() {
    assert_eq!(
        format!("{:?}", read(SubqueryRead::Scalar, &[]).unwrap()),
        format!("{:?}", Value::Null)
    );
    assert_eq!(
        format!("{:?}", read(SubqueryRead::Scalar, &[Some(4.)]).unwrap()),
        format!("{:?}", f(Some(4.)))
    );
    assert!(read(SubqueryRead::Scalar, &[Some(4.), Some(5.)]).is_err());
    assert!(matches!(
        read(SubqueryRead::Exists, &[]),
        Ok(Value::Bool(false))
    ));
    assert!(matches!(
        read(SubqueryRead::Exists, &[None]),
        Ok(Value::Bool(true))
    ));
}

// PromQL `scalar(v)` is the value of v's one sample, NaN with none or more.
#[test]
fn scalar_of_a_vector_is_nan_unless_one_sample() {
    let input = schema(&[("x", DataType::Float64)]);
    let vector = Arc::new(Schema::new(vec![Field::plain(
        "value",
        DataType::Float64,
        false,
    )]));
    for (samples, expected) in [(vec![], f64::NAN), (vec![2.], 2.), (vec![2., 3.], f64::NAN)] {
        let operator = Operator::bind_subquery(
            input.clone(),
            vector.clone(),
            SubqueryRead::VectorScalar { value: 0 },
            "scalar",
        )
        .unwrap();
        let rows = run(
            vec![
                (input.clone(), vec![vec![f(Some(1.))]]),
                (
                    vector.clone(),
                    samples.iter().map(|v| vec![Value::Float64(*v)]).collect(),
                ),
            ],
            operator,
        )
        .unwrap();
        let Value::Float64(value) = rows[0][1] else {
            panic!()
        };
        assert!(
            value == expected || (value.is_nan() && expected.is_nan()),
            "{samples:?}"
        );
    }
}

// `x IN (subquery)`: true on a match; else NULL when x or a value is NULL;
// false over no value, even for a NULL x; NaN matches nothing. An integer
// probe compares to floats as a float.
#[test]
fn in_subquery_follows_sql_three_valued_logic() {
    let probe = |x: Option<f64>, values: &[Option<f64>]| -> Option<bool> {
        let input = schema(&[("x", DataType::Float64)]);
        let subquery = schema(&[("value", DataType::Float64)]);
        let probe = Expression::planner(
            CompiledExpression::compile(&planner_types::ir::ScalarExpr::Column(0), &input).unwrap(),
        );
        let operator = Operator::bind_subquery(
            input.clone(),
            subquery.clone(),
            SubqueryRead::In { probe },
            "in",
        )
        .unwrap();
        let rows = run(
            vec![
                (input, vec![vec![f(x)]]),
                (subquery, values.iter().map(|v| vec![f(*v)]).collect()),
            ],
            operator,
        )
        .unwrap();
        match rows[0][1] {
            Value::Bool(b) => Some(b),
            Value::Null => None,
            ref other => panic!("{other:?}"),
        }
    };
    assert_eq!(probe(Some(1.), &[Some(2.), Some(1.)]), Some(true));
    assert_eq!(probe(Some(1.), &[Some(2.)]), Some(false));
    assert_eq!(probe(Some(1.), &[Some(2.), None]), None);
    assert_eq!(probe(None, &[Some(2.)]), None);
    assert_eq!(probe(None, &[]), Some(false));
    assert_eq!(probe(Some(f64::NAN), &[Some(f64::NAN)]), Some(false));
    assert_eq!(probe(Some(-0.), &[Some(0.)]), Some(true));
    // Int64 against Float64.
    let input = schema(&[("x", DataType::Int64)]);
    let subquery = schema(&[("value", DataType::Float64)]);
    let probe = Expression::planner(
        CompiledExpression::compile(&planner_types::ir::ScalarExpr::Column(0), &input).unwrap(),
    );
    let operator = Operator::bind_subquery(
        input.clone(),
        subquery.clone(),
        SubqueryRead::In { probe },
        "in",
    )
    .unwrap();
    let rows = run(
        vec![
            (input, vec![vec![i(2)]]),
            (subquery, vec![vec![f(Some(2.))]]),
        ],
        operator,
    )
    .unwrap();
    assert!(matches!(rows[0][1], Value::Bool(true)));
}

// `limitk by (g)` keeps each group's first k series in label order; a NaN
// `limit_ratio` is bad input.
#[test]
fn limitk_keeps_each_groups_first_series() {
    let input = Arc::new(Schema::new(vec![
        Field::plain("g", DataType::Utf8, true),
        Field::plain("i", DataType::Utf8, true),
        Field::plain("value", DataType::Float64, false),
    ]));
    let text = |s: &str| Value::Utf8(s.into());
    let rows = [("x", "c"), ("y", "b"), ("x", "a"), ("y", "a"), ("x", "b")]
        .iter()
        .map(|(g, s)| vec![text(g), text(s), Value::Float64(1.)])
        .collect();
    let operator = Operator::series_sample(input.clone(), vec![0], SampleKind::LimitK(2)).unwrap();
    let kept = run(vec![(input.clone(), rows)], operator).unwrap();
    assert_eq!(
        debug(&kept),
        debug(
            &[("x", "a"), ("x", "b"), ("y", "a"), ("y", "b")]
                .iter()
                .map(|(g, s)| vec![text(g), text(s), Value::Float64(1.)])
                .collect::<Vec<_>>()
        )
    );
    assert!(Operator::series_sample(input, vec![], SampleKind::LimitRatio(f64::NAN)).is_err());
}
