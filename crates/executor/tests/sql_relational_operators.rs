//! SQL relational operators (set operations, `DISTINCT`, window
//! functions) and SQL subquery reads, each against the values its SQL
//! definition gives, edge cases included (NULL, NaN, empty input, ties).
use asap_executor::{
    expressions::CompiledExpression,
    operators::{Expression, Operator, Reduction, ScalarRefRead, SortKey, SqlWindowSpec},
    physical_planner::compile_node,
    values::{SchemaRef, Value},
    Error,
};
use planner_types::ir::operator::{
    RelationalSetOpKind, WindowFrame, WindowFrameBound, WindowFrameOffset, WindowFrameUnits,
    WindowFuncKind,
};
use planner_types::ir::scalar::ScalarValue;
use planner_types::ir::schema::{DataType, Field, Schema};
use planner_types::ir::NonASAPOp;
use std::sync::Arc;

fn schema(fields: &[(&str, DataType)]) -> SchemaRef {
    Arc::new(Schema::new(
        fields
            .iter()
            .map(|(name, dtype)| Field::plain(*name, dtype.clone(), true))
            .collect(),
    ))
}

fn run(
    inputs: Vec<(SchemaRef, Vec<Vec<Value>>)>,
    operator: Operator,
) -> Result<Vec<Vec<Value>>, Error> {
    super::common::run_over_sources(inputs, operator)
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
    use RelationalSetOpKind::{Except, Intersect};
    for (kind, all, expected) in [
        (Intersect, false, rows(&[Some(1.), None])),
        (Intersect, true, rows(&[Some(1.), Some(1.), None])),
        (Except, false, rows(&[Some(2.), Some(3.)])),
        (Except, true, rows(&[Some(1.), Some(2.), None, Some(3.)])),
    ] {
        let operator =
            Operator::set_operation(input.clone(), input.clone(), kind.clone(), all).unwrap();
        let actual = run(
            vec![
                (input.clone(), left.clone()),
                (input.clone(), right.clone()),
            ],
            operator,
        )
        .unwrap();
        assert_eq!(debug(&actual), debug(&expected), "{kind:?}, all {all}");
    }
    // Over an empty right input, INTERSECT is empty and EXCEPT distinct.
    let empty = |kind| {
        run(
            vec![(input.clone(), left.clone()), (input.clone(), vec![])],
            Operator::set_operation(input.clone(), input.clone(), kind, false).unwrap(),
        )
        .unwrap()
    };
    assert!(empty(Intersect).is_empty());
    assert_eq!(empty(Except).len(), 4);
    // A UNION lowers to a union operator, not a set operation.
    assert!(
        Operator::set_operation(input.clone(), input, RelationalSetOpKind::Union, false).is_err()
    );
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
        SqlWindowSpec {
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
fn read(read: ScalarRefRead, values: &[Option<f64>]) -> Result<Value, Error> {
    let input = schema(&[("x", DataType::Float64)]);
    let subquery = schema(&[("value", DataType::Float64)]);
    let operator =
        Operator::bind_scalar_ref(input.clone(), subquery.clone(), read, "bound").unwrap();
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
        format!("{:?}", read(ScalarRefRead::Scalar, &[]).unwrap()),
        format!("{:?}", Value::Null)
    );
    assert_eq!(
        format!("{:?}", read(ScalarRefRead::Scalar, &[Some(4.)]).unwrap()),
        format!("{:?}", f(Some(4.)))
    );
    assert!(read(ScalarRefRead::Scalar, &[Some(4.), Some(5.)]).is_err());
    assert!(matches!(
        read(ScalarRefRead::Exists, &[]),
        Ok(Value::Bool(false))
    ));
    assert!(matches!(
        read(ScalarRefRead::Exists, &[None]),
        Ok(Value::Bool(true))
    ));
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
        let operator = Operator::bind_scalar_ref(
            input.clone(),
            subquery.clone(),
            ScalarRefRead::In { probe },
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
    let operator = Operator::bind_scalar_ref(
        input.clone(),
        subquery.clone(),
        ScalarRefRead::In { probe },
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

/// `spec` over `rows` of `input`: the window column, in input order.
fn window_column(
    input: &SchemaRef,
    spec: SqlWindowSpec,
    rows: Vec<Vec<Value>>,
) -> Result<Vec<Value>, Error> {
    let operator = Operator::sql_window(input.clone(), spec, "w")?;
    Ok(run(vec![(input.clone(), rows)], operator)?
        .into_iter()
        .map(|mut row| row.pop().unwrap())
        .collect())
}

/// `function` of column `argument`, in one partition ordered by column
/// `order` (none when `None`), over `frame`.
fn spec(
    function: WindowFuncKind,
    argument: Option<usize>,
    partition: Vec<usize>,
    order: Option<usize>,
    frame: Option<WindowFrame>,
) -> SqlWindowSpec {
    SqlWindowSpec {
        function,
        argument,
        offset: 1,
        default: Value::Null,
        partition,
        order: order
            .map(|column| SortKey {
                column,
                descending: false,
                nulls_first: false,
            })
            .into_iter()
            .collect(),
        frame,
    }
}

/// `reduction` of `rows` of `input` by the `Aggregate` into no groups: its
/// value, NULL (a count 0) over no row.
fn aggregate_value(input: &SchemaRef, reduction: Reduction, rows: Vec<Vec<Value>>) -> Value {
    let empty = rows.is_empty();
    let operator = Operator::aggregate(input.clone(), vec![], vec![("m".into(), reduction)])
        .unwrap()
        .with_emit_empty_global(true, None)
        .unwrap();
    let mut out = run(vec![(input.clone(), rows)], operator).unwrap();
    assert!(empty || out.len() == 1);
    out.pop().map_or(Value::Null, |mut row| row.pop().unwrap())
}

fn shown(values: &[Value]) -> Vec<String> {
    values.iter().map(|value| format!("{value:?}")).collect()
}

// A window aggregate reduces its frame's rows as the `Aggregate` reduces
// them: MAX and MIN over {1, NaN} are 1 (NaN only over NaN alone); an
// Int64 SUM fails only when its result leaves Int64, not a partial sum
// (MAX + 1 - 1 is MAX); SUM over {inf, 1} is inf, over {inf, -inf} NaN.
#[test]
fn window_aggregates_reduce_as_the_aggregate_does() {
    use WindowFuncKind as F;
    let floats = schema(&[("o", DataType::Float64), ("v", DataType::Float64)]);
    let float_rows = |values: &[f64]| -> Vec<Vec<Value>> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| vec![f(Some(i as f64)), f(Some(*v))])
            .collect()
    };
    for (function, reduction) in [(F::Max, Reduction::Max(1)), (F::Min, Reduction::Min(1))] {
        for values in [[1., f64::NAN], [f64::NAN, 1.]] {
            let rows = float_rows(&values);
            let whole = window_column(
                &floats,
                spec(function.clone(), Some(1), vec![], None, None),
                rows.clone(),
            )
            .unwrap();
            let aggregate = aggregate_value(&floats, reduction.clone(), rows.clone());
            assert_eq!(
                shown(&whole),
                shown(&[aggregate.clone(), aggregate]),
                "{function:?} {values:?}"
            );
            // Up to each row: {first}, then both.
            let running = window_column(
                &floats,
                spec(function.clone(), Some(1), vec![], Some(0), None),
                rows.clone(),
            )
            .unwrap();
            assert_eq!(
                shown(&running),
                shown(&[
                    aggregate_value(&floats, reduction.clone(), rows[..1].to_vec()),
                    aggregate_value(&floats, reduction.clone(), rows),
                ]),
                "{function:?} {values:?}"
            );
        }
    }
    for values in [[f64::INFINITY, 1.], [f64::INFINITY, f64::NEG_INFINITY]] {
        let rows = float_rows(&values);
        let whole = window_column(
            &floats,
            spec(F::Sum, Some(1), vec![], None, None),
            rows.clone(),
        )
        .unwrap();
        let aggregate = aggregate_value(&floats, Reduction::Sum(1), rows);
        assert_eq!(shown(&whole), shown(&[aggregate.clone(), aggregate]));
    }
    let integers = schema(&[("v", DataType::Int64)]);
    let rows = vec![vec![i(i64::MAX)], vec![i(1)], vec![i(-1)]];
    for (function, reduction) in [(F::Sum, Reduction::Sum(0)), (F::Avg, Reduction::Avg(0))] {
        let whole = window_column(
            &integers,
            spec(function.clone(), Some(0), vec![], None, None),
            rows.clone(),
        )
        .unwrap();
        let aggregate = aggregate_value(&integers, reduction, rows.clone());
        assert_eq!(
            shown(&whole),
            shown(&[aggregate.clone(), aggregate.clone(), aggregate])
        );
    }
    assert_eq!(
        shown(
            &window_column(
                &integers,
                spec(F::Sum, Some(0), vec![], None, None),
                rows.clone()
            )
            .unwrap()
        ),
        shown(&[i(i64::MAX), i(i64::MAX), i(i64::MAX)])
    );
    // A frame whose sum leaves Int64 fails, as the aggregate does.
    let overflowing = vec![vec![i(i64::MAX)], vec![i(1)]];
    assert!(window_column(
        &integers,
        spec(F::Sum, Some(0), vec![], None, None),
        overflowing
    )
    .is_err());
}

/// Rows of `(partition, order key, value)` for the sliding frame checks:
/// two partitions, order keys with ties, values small integers (so every
/// sum is exact) with NULLs, NaNs and infinities.
fn sliding_rows(n: usize) -> Vec<Vec<Value>> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..n)
        .map(|row| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let draw = (state >> 33) % 100;
            let value = match draw {
                0..=7 => Value::Null,
                8..=9 => f(Some(f64::NAN)),
                10 => f(Some(f64::INFINITY)),
                11 => f(Some(f64::NEG_INFINITY)),
                draw => f(Some(draw as f64 - 50.)),
            };
            vec![i((row % 2) as i64), f(Some((row / 6) as f64)), value]
        })
        .collect()
}

// Sliding frames (rows enter at the end, leave at the start) give each
// row the value of its frame reduced alone by the `Aggregate`, for frames
// whose start moves: `ROWS 3 PRECEDING AND 2 FOLLOWING`, `ROWS CURRENT ROW
// AND UNBOUNDED FOLLOWING`, `ROWS 5 PRECEDING AND 2 PRECEDING` (empty at a
// partition's start).
#[test]
fn sliding_frames_reduce_each_frame_as_the_aggregate_does() {
    use WindowFrameUnits::Rows;
    use WindowFuncKind as F;
    let input = schema(&[
        ("p", DataType::Int64),
        ("o", DataType::Float64),
        ("v", DataType::Float64),
    ]);
    let rows = sliding_rows(60);
    // Each partition's rows in window order (ties keep input order).
    let partition = |p: i64| -> Vec<usize> {
        let mut members: Vec<usize> = (0..rows.len())
            .filter(|&row| matches!(rows[row][0], Value::Int64(q) if q == p))
            .collect();
        members.sort_by(|&a, &b| rows[a][1].compare(&rows[b][1]).unwrap());
        members
    };
    let partitions = [partition(0), partition(1)];
    // Each frame, and its first and last position for position `i` of a
    // partition of `n` rows.
    type FrameBounds = fn(i64, i64) -> (i64, i64);
    let frames: [(WindowFrame, FrameBounds); 3] = [
        (frame(Rows, preceding(3), following(2)), |i, n| {
            ((i - 3).max(0), (i + 2).min(n - 1))
        }),
        (
            frame(Rows, WindowFrameBound::CurrentRow, unbounded_following()),
            |i, n| (i, n - 1),
        ),
        (frame(Rows, preceding(5), preceding(2)), |i, _| {
            ((i - 5).max(0), i - 2)
        }),
    ];
    let functions = [
        (F::Sum, Some(2), Some(Reduction::Sum(2))),
        (F::Avg, Some(2), Some(Reduction::Avg(2))),
        (F::Min, Some(2), Some(Reduction::Min(2))),
        (F::Max, Some(2), Some(Reduction::Max(2))),
        (F::Count, None, Some(Reduction::Count)),
        (F::Count, Some(2), None),
    ];
    for (window_frame, bounds) in &frames {
        for (function, argument, reduction) in &functions {
            let actual = window_column(
                &input,
                spec(
                    function.clone(),
                    *argument,
                    vec![0],
                    Some(1),
                    Some(window_frame.clone()),
                ),
                rows.clone(),
            )
            .unwrap();
            for members in &partitions {
                let n = members.len() as i64;
                for (position, &row) in members.iter().enumerate() {
                    let (start, end) = bounds(position as i64, n);
                    let frame_rows: Vec<Vec<Value>> = (start..=end)
                        .map(|p| rows[members[p as usize]].clone())
                        .collect();
                    let expected = match reduction {
                        Some(reduction) => aggregate_value(&input, reduction.clone(), frame_rows),
                        // COUNT(v): the frame's non-NULL values.
                        None => i(frame_rows
                            .iter()
                            .filter(|r| !matches!(r[2], Value::Null))
                            .count() as i64),
                    };
                    assert_eq!(
                        format!("{:?}", actual[row]),
                        format!("{expected:?}"),
                        "{function:?}({argument:?}) over {window_frame:?}, row {row}"
                    );
                }
            }
        }
    }
}

// Frames whose start moves take linear time: over 1M rows, `max(v) OVER
// (ORDER BY o ROWS 1000 PRECEDING)` and `sum(v) OVER (ORDER BY o ROWS
// BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING)` each finish in seconds
// (they recomputed every frame before, minutes). Slow in a debug build:
// `cargo test --release -p asap-executor --test executor
// sliding_frames_over_a_million_rows -- --ignored --nocapture`.
#[test]
#[ignore = "1M-row timing check; run with --ignored"]
fn sliding_frames_over_a_million_rows() {
    use WindowFrameUnits::Rows;
    use WindowFuncKind as F;
    let input = schema(&[("o", DataType::Int64), ("v", DataType::Float64)]);
    let n = 1_000_000;
    let rows: Vec<Vec<Value>> = (0..n)
        .map(|row| vec![i(row), f(Some(((row * 7_919) % 1_000) as f64))])
        .collect();
    for (name, function, window_frame) in [
        (
            "max ROWS 1000 PRECEDING",
            F::Max,
            frame(Rows, preceding(1000), WindowFrameBound::CurrentRow),
        ),
        (
            "sum ROWS CURRENT ROW..UNBOUNDED FOLLOWING",
            F::Sum,
            frame(Rows, WindowFrameBound::CurrentRow, unbounded_following()),
        ),
    ] {
        let started = std::time::Instant::now();
        let operator = Operator::sql_window(
            input.clone(),
            spec(function, Some(1), vec![], Some(0), Some(window_frame)),
            "w",
        )
        .unwrap();
        let values = super::common::run_over_sources_within(
            asap_executor::runtime::Limits { max_bytes: 1 << 30 },
            vec![(input.clone(), rows.clone())],
            operator,
        )
        .unwrap();
        let elapsed = started.elapsed();
        println!("{name}: {n} rows in {:.3} s", elapsed.as_secs_f64());
        assert_eq!(values.len(), n as usize);
        assert!(elapsed.as_secs() < 60, "{name} took {elapsed:?}");
    }
}

// `compile_node` compiles a node one operator implements: `SELECT
// DISTINCT a` (a `Dedup`) keeps each distinct value once; `INTERSECT`
// (a `SetOp`) the rows both sides have.
#[test]
fn compile_node_compiles_dedup_and_set_operations() {
    let input = schema(&[("a", DataType::Float64)]);
    let rows = |values: &[f64]| values.iter().map(|v| vec![f(Some(*v))]).collect::<Vec<_>>();
    let dedup = compile_node(
        &super::common::planner_node(
            NonASAPOp::Dedup {
                cols: vec![],
                child: 0,
            },
            &input,
        ),
        std::slice::from_ref(&input),
        true,
    )
    .unwrap();
    assert_eq!(
        debug(&run(vec![(input.clone(), rows(&[2., 1., 2., 1.]))], dedup).unwrap()),
        debug(&rows(&[1., 2.]))
    );
    let intersect = compile_node(
        &super::common::planner_node(
            NonASAPOp::SetOp {
                kind: RelationalSetOpKind::Intersect,
                all: false,
                left: 0,
                right: 1,
            },
            &input,
        ),
        &[input.clone(), input.clone()],
        true,
    )
    .unwrap();
    assert_eq!(
        debug(
            &run(
                vec![
                    (input.clone(), rows(&[1., 2., 3.])),
                    (input.clone(), rows(&[3., 1.]))
                ],
                intersect
            )
            .unwrap()
        ),
        debug(&rows(&[1., 3.]))
    );
}

// A `RANGE` offset frame over NULL order keys: NULL sorts before every
// number (`NULLS FIRST`), so it precedes every limit. Over keys NULL, 8,
// 8, 5 (descending), `RANGE BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING`
// counts the NULL row's peers (1), the NULL row for each 8 (1), and the
// NULL and both 8s for 5 (3), as DuckDB does.
#[test]
fn range_offsets_place_null_keys_before_every_number() {
    let input = schema(&[("o", DataType::Float64)]);
    let rows = [None, Some(8.), Some(8.), Some(5.)]
        .into_iter()
        .map(|o| vec![f(o)])
        .collect();
    let mut window = spec(
        WindowFuncKind::Count,
        None,
        vec![],
        Some(0),
        Some(frame(
            WindowFrameUnits::Range,
            WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Null)),
            preceding(1),
        )),
    );
    window.order[0].descending = true;
    window.order[0].nulls_first = true;
    assert_eq!(
        shown(&window_column(&input, window, rows).unwrap()),
        shown(&[i(1), i(1), i(1), i(3)])
    );
}
