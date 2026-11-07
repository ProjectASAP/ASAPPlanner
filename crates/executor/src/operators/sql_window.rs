//! SQL analytic window functions (`f(…) OVER (PARTITION BY … ORDER BY …
//! frame)`): each row gets its function's value over its partition, as
//! DataFusion computes it.
use super::*;
use planner_types::ir::operator::{
    WindowFrame, WindowFrameBound, WindowFrameOffset, WindowFrameUnits, WindowFuncKind,
};
use planner_types::ir::scalar::ScalarValue;

/// One window function over its input's partitions.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WindowSpec {
    pub function: WindowFuncKind,
    /// The column of the function's value argument; `None` for the ranking
    /// functions and `COUNT(*)`.
    pub argument: Option<usize>,
    /// `LAG` / `LEAD`'s offset (1 by default).
    pub offset: i64,
    /// `LAG` / `LEAD`'s value past the partition or frame (NULL by default).
    pub default: Value,
    /// The partition columns.
    pub partition: Vec<usize>,
    /// The order within a partition.
    pub order: Vec<SortKey>,
    /// The rows around a row its function reads; `None` is SQL's default:
    /// `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` with an order,
    /// the whole partition without.
    pub frame: Option<WindowFrame>,
}

impl Operator {
    /// `input` with one more column, `name`: each row's `spec` value.
    pub fn sql_window(input: SchemaRef, spec: WindowSpec, name: &str) -> Result<Self, Error> {
        use WindowFuncKind as F;
        validate_groups(&input, &spec.partition)?;
        for key in &spec.order {
            if !ordered(plain(&input, key.column)?.0) {
                return Err(invalid("unsupported window order type"));
            }
        }
        let argument = spec
            .argument
            .map(|column| plain(&input, column).map(|(dtype, _)| dtype.clone()))
            .transpose()?;
        let numeric = |dtype: &DataType| matches!(dtype, DataType::Int64 | DataType::Float64);
        let (dtype, nullable) = match (&spec.function, argument) {
            (F::RowNumber | F::Rank | F::DenseRank, _) | (F::Count, _) => (DataType::Int64, false),
            (F::Avg, Some(dtype)) if numeric(&dtype) => (DataType::Float64, true),
            (F::Sum, Some(dtype)) if numeric(&dtype) => (dtype, true),
            (
                F::Lag
                | F::Lead
                | F::LagInFrame
                | F::LeadInFrame
                | F::FirstValue
                | F::LastValue
                | F::NthValue(_)
                | F::Min
                | F::Max,
                Some(dtype),
            ) => (dtype, true),
            (function, _) => {
                return Err(invalid(&format!(
                    "window function {function:?} over this argument has no native implementation"
                )))
            }
        };
        if !matches!(spec.default, Value::Null) && !spec.default.matches(&dtype, true) {
            return Err(invalid(
                "window function default differs from its value type",
            ));
        }
        if matches!(spec.function, F::NthValue(None | Some(0))) {
            return Err(invalid("nth_value needs a positive n"));
        }
        if let Some(frame) = &spec.frame {
            let offsets = [&frame.start_bound, &frame.end_bound]
                .into_iter()
                .filter_map(|bound| match bound {
                    WindowFrameBound::Preceding(offset) | WindowFrameBound::Following(offset) => {
                        Some(offset)
                    }
                    WindowFrameBound::CurrentRow => None,
                });
            for offset in offsets {
                frame_offset(frame, offset, &spec.order, &input)?;
            }
        }
        let mut fields = input.fields.clone();
        fields.push(result_field(name, dtype, nullable));
        let output = Arc::new(Schema {
            fields,
            unique_keys: vec![],
            closed: false,
            time_index: input.time_index,
        });
        Ok(Self {
            kind: Kind::SqlWindow(Box::new(spec)),
            inputs: vec![input],
            output,
        })
    }
}

/// How far a frame bound reaches: rows for a `ROWS` frame; a distance on
/// the one order key for a `RANGE` frame (milliseconds over a timestamp).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reach {
    Rows(i64),
    Distance(f64),
}

/// The reach of a bound's `offset`; `None` when unbounded.
fn reach(units: &WindowFrameUnits, offset: &WindowFrameOffset) -> Result<Option<Reach>, Error> {
    Ok(Some(match (units, offset) {
        (_, WindowFrameOffset::Scalar(ScalarValue::Null)) => return Ok(None),
        (WindowFrameUnits::Rows, WindowFrameOffset::Scalar(ScalarValue::Int64(rows)))
            if *rows >= 0 =>
        {
            Reach::Rows(*rows)
        }
        (WindowFrameUnits::Range, WindowFrameOffset::Scalar(ScalarValue::Int64(v))) if *v >= 0 => {
            Reach::Distance(*v as f64)
        }
        (WindowFrameUnits::Range, WindowFrameOffset::Scalar(ScalarValue::Float64(v)))
            if *v >= 0.0 =>
        {
            Reach::Distance(*v)
        }
        (
            WindowFrameUnits::Range,
            WindowFrameOffset::Interval {
                months: 0,
                days,
                nanoseconds,
            },
        ) if *days >= 0 && *nanoseconds >= 0 && nanoseconds % 1_000_000 == 0 => {
            Reach::Distance(*days as f64 * 86_400_000.0 + (*nanoseconds / 1_000_000) as f64)
        }
        _ => return Err(invalid("unsupported window frame offset")),
    }))
}

/// Check a bound's offset against the order: a `RANGE` distance needs one
/// order key, numeric for a number, a timestamp for an interval.
fn frame_offset(
    frame: &WindowFrame,
    offset: &WindowFrameOffset,
    order: &[SortKey],
    input: &SchemaRef,
) -> Result<(), Error> {
    if reach(&frame.units, offset)?.is_none() || frame.units == WindowFrameUnits::Rows {
        return Ok(());
    }
    let [key] = order else {
        return Err(invalid("a RANGE frame offset needs exactly one order key"));
    };
    match (plain(input, key.column)?.0, offset) {
        (DataType::Int64 | DataType::Float64, WindowFrameOffset::Scalar(_))
        | (DataType::Timestamp, WindowFrameOffset::Interval { .. }) => Ok(()),
        _ => Err(invalid(
            "a RANGE frame offset is a number over a numeric key or an interval over a timestamp",
        )),
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::SqlWindow(spec) = &operator.kind else {
        unreachable!()
    };
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let collected = collect_batch(input, &operator.inputs[0], &context).await?;
        let batch = &collected.batch;
        let rows = batch.num_rows();
        let mut workspace = Workspace::new(&context)?;
        let mut table = super::group_table::GroupTable::new(spec.partition.clone());
        let group_of_row = table.group_ids(batch)?;
        let keys = spec
            .order
            .iter()
            .map(|key| decoded(batch, key.column))
            .collect::<Result<Vec<_>, _>>()?;
        let arguments = spec
            .argument
            .map(|column| decoded(batch, column))
            .transpose()?
            .unwrap_or_default();
        workspace.set_bytes(
            table.memory_bytes()
                + rows * 16
                + keys
                    .iter()
                    .chain([&arguments])
                    .flatten()
                    .map(Value::bytes)
                    .sum::<usize>(),
        )?;
        // Rows by partition, then by the order keys; ties keep input order.
        let order = super::sort::cooperative_sort(
            (0..rows).collect::<Vec<_>>(),
            |&a, &b| {
                group_of_row[a].cmp(&group_of_row[b]).then_with(|| {
                    spec.order
                        .iter()
                        .zip(&keys)
                        .map(|(key, values)| super::sort::compare_key(&values[a], &values[b], key))
                        .find(|order| order.is_ne())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            },
            &context,
        )
        .await?;
        let (dtype, _) = plain(&operator.output, operator.output.fields.len() - 1)?;
        let mut results = vec![Value::Null; rows];
        let mut work = Cooperative::new(&context);
        let mut start = 0;
        while start < rows {
            let group = group_of_row[order[start]];
            let end = start + order[start..].partition_point(|&row| group_of_row[row] == group);
            let partition = &order[start..end];
            for (position, value) in evaluate_partition(spec, partition, &keys, &arguments, dtype)?
                .into_iter()
                .enumerate()
            {
                results[partition[position]] = value;
            }
            work.checkpoint_rows(partition.len()).await?;
            start = end;
        }
        let column = crate::values::arrow_columns::encode(
            dtype,
            crate::values::arrow_columns::ColumnDepth::BatchColumn,
            &results.iter().collect::<Vec<_>>(),
        )?;
        let mut arrays = batch.record_batch().columns().to_vec();
        let mut states: Vec<_> = (0..arrays.len())
            .map(|column| batch.state_column(column).cloned())
            .collect();
        arrays.push(column);
        states.push(None);
        Batch::from_parts(operator.output.clone(), arrays, states, rows)
    })
    .boxed_local())
}

/// Each row's value of `spec` over one partition: `partition` holds its
/// rows in window order; `keys` and `arguments` are indexed by row.
fn evaluate_partition(
    spec: &WindowSpec,
    partition: &[usize],
    keys: &[Vec<Value>],
    arguments: &[Value],
    dtype: &DataType,
) -> Result<Vec<Value>, Error> {
    use WindowFuncKind as F;
    let n = partition.len();
    let peers = |a: usize, b: usize| {
        spec.order.iter().zip(keys).all(|(key, values)| {
            super::sort::compare_key(&values[partition[a]], &values[partition[b]], key).is_eq()
        })
    };
    // Each position's peer group: its first and last position.
    let mut first_peer = vec![0; n];
    let mut last_peer = vec![0; n];
    for i in 1..n {
        first_peer[i] = if peers(i - 1, i) {
            first_peer[i - 1]
        } else {
            i
        };
    }
    for i in (0..n).rev() {
        last_peer[i] = if i + 1 < n && peers(i, i + 1) {
            last_peer[i + 1]
        } else {
            i
        };
    }
    let argument = |position: usize| arguments[partition[position]].clone();
    match &spec.function {
        F::RowNumber => return Ok((1..=n as i64).map(Value::Int64).collect()),
        F::Rank => {
            return Ok(first_peer
                .iter()
                .map(|&first| Value::Int64(first as i64 + 1))
                .collect())
        }
        F::DenseRank => {
            let mut rank = 0;
            return Ok((0..n)
                .map(|i| {
                    if first_peer[i] == i {
                        rank += 1;
                    }
                    Value::Int64(rank)
                })
                .collect());
        }
        F::Lag | F::Lead => {
            let step = if spec.function == F::Lag {
                -spec.offset
            } else {
                spec.offset
            };
            return Ok((0..n)
                .map(|i| {
                    match (i as i64)
                        .checked_add(step)
                        .filter(|&j| j >= 0 && j < n as i64)
                    {
                        Some(j) => argument(j as usize),
                        None => spec.default.clone(),
                    }
                })
                .collect());
        }
        _ => {}
    }
    let frames = frames(spec, partition, keys, &first_peer, &last_peer)?;
    let counts_rows = spec.argument.is_none();
    let mut results = Vec::with_capacity(n);
    // The aggregate of the positions `from..to`, kept while frames share
    // their start: a frame's end only grows, so it extends.
    let mut running: Option<(i64, i64, Accumulator)> = None;
    for (i, &(start, end)) in frames.iter().enumerate() {
        let in_frame = |position: i64| (start..=end).contains(&position);
        let at = |position: i64| argument(position as usize);
        results.push(match &spec.function {
            F::FirstValue if start <= end => at(start),
            F::LastValue if start <= end => at(end),
            F::FirstValue | F::LastValue => Value::Null,
            F::NthValue(Some(nth)) => {
                let position = start.saturating_add(*nth as i64 - 1);
                if in_frame(position) {
                    at(position)
                } else {
                    Value::Null
                }
            }
            F::LagInFrame | F::LeadInFrame => {
                let step = if spec.function == F::LagInFrame {
                    -spec.offset
                } else {
                    spec.offset
                };
                match (i as i64).checked_add(step) {
                    Some(position) if in_frame(position) => at(position),
                    _ => spec.default.clone(),
                }
            }
            F::Sum | F::Avg | F::Count | F::Min | F::Max => {
                let (from, mut accumulator) = match running.take() {
                    Some((from, to, accumulator)) if from == start => (to, accumulator),
                    _ => (start, Accumulator::default()),
                };
                for position in from..=end {
                    accumulator.add(arguments.get(partition[position as usize]))?;
                }
                let value = accumulator.value(&spec.function, dtype, counts_rows);
                running = Some((start, end.max(from - 1) + 1, accumulator));
                value
            }
            _ => unreachable!("ranking and offset functions returned above"),
        });
    }
    Ok(results)
}

/// A frame's aggregate: its rows, and its non-null values' count, sum and
/// extremes.
#[derive(Default)]
struct Accumulator {
    rows: i64,
    count: i64,
    integer_sum: i64,
    float_sum: f64,
    min: Option<Value>,
    max: Option<Value>,
}

impl Accumulator {
    /// Add one row's value; `None` for `COUNT(*)`, which reads none.
    fn add(&mut self, value: Option<&Value>) -> Result<(), Error> {
        self.rows += 1;
        let Some(value) = value.filter(|value| !matches!(value, Value::Null)) else {
            return Ok(());
        };
        match value {
            Value::Int64(v) => {
                self.integer_sum = self
                    .integer_sum
                    .checked_add(*v)
                    .ok_or_else(|| Error::Operator("window sum overflows Int64".into()))?;
                self.float_sum += *v as f64;
            }
            Value::Float64(v) => self.float_sum += v,
            _ => {}
        }
        self.count += 1;
        // NaN is the largest float, as DataFusion's MIN and MAX order it.
        if self
            .min
            .as_ref()
            .is_none_or(|min| value.compare(min).is_ok_and(|o| o.is_lt()))
        {
            self.min = Some(value.clone());
        }
        if self
            .max
            .as_ref()
            .is_none_or(|max| value.compare(max).is_ok_and(|o| o.is_gt()))
        {
            self.max = Some(value.clone());
        }
        Ok(())
    }

    /// `function` of the rows added; NULL over no value, but a count.
    fn value(&self, function: &WindowFuncKind, dtype: &DataType, counts_rows: bool) -> Value {
        use WindowFuncKind as F;
        match function {
            F::Count if counts_rows => Value::Int64(self.rows),
            F::Count => Value::Int64(self.count),
            _ if self.count == 0 => Value::Null,
            F::Sum if *dtype == DataType::Int64 => Value::Int64(self.integer_sum),
            F::Sum => Value::Float64(self.float_sum),
            F::Avg => Value::Float64(self.float_sum / self.count as f64),
            F::Min => self.min.clone().unwrap_or(Value::Null),
            F::Max => self.max.clone().unwrap_or(Value::Null),
            _ => unreachable!("an aggregate window function"),
        }
    }
}

/// Each position's frame in `partition`, as first and last position
/// (empty when first > last).
fn frames(
    spec: &WindowSpec,
    partition: &[usize],
    keys: &[Vec<Value>],
    first_peer: &[usize],
    last_peer: &[usize],
) -> Result<Vec<(i64, i64)>, Error> {
    let n = partition.len() as i64;
    let unbounded = || WindowFrameOffset::Scalar(ScalarValue::Null);
    let default = WindowFrame {
        units: if spec.order.is_empty() {
            WindowFrameUnits::Rows
        } else {
            WindowFrameUnits::Range
        },
        start_bound: WindowFrameBound::Preceding(unbounded()),
        end_bound: if spec.order.is_empty() {
            WindowFrameBound::Following(unbounded())
        } else {
            WindowFrameBound::CurrentRow
        },
    };
    let frame = spec.frame.as_ref().unwrap_or(&default);
    let descending = spec.order.first().is_some_and(|key| key.descending);
    // The one order key's value at a position, negated for a descending
    // order so that it grows in window order; `None` for NULL and NaN,
    // which frame only their peers.
    let number = |position: usize| -> Option<f64> {
        let value = match &keys.first()?[partition[position]] {
            Value::Int64(v) => *v as f64,
            Value::Float64(v) if !v.is_nan() => *v,
            Value::Timestamp(v) => *v as f64,
            _ => return None,
        };
        Some(if descending { -value } else { value })
    };
    // The positions with a number: one run in window order, by number.
    let numbered: Vec<usize> = (0..partition.len())
        .filter(|&p| number(p).is_some())
        .collect();
    let bound = |i: usize, bound: &WindowFrameBound, is_start: bool| -> Result<i64, Error> {
        let (offset, forward) = match bound {
            WindowFrameBound::CurrentRow => {
                return Ok(match (frame.units == WindowFrameUnits::Rows, is_start) {
                    (true, _) => i as i64,
                    (false, true) => first_peer[i] as i64,
                    (false, false) => last_peer[i] as i64,
                })
            }
            WindowFrameBound::Preceding(offset) => (offset, false),
            WindowFrameBound::Following(offset) => (offset, true),
        };
        Ok(match reach(&frame.units, offset)? {
            None if forward != is_start => {
                if forward {
                    n - 1
                } else {
                    0
                }
            }
            None => return Err(invalid("a frame bound is unbounded the wrong way")),
            Some(Reach::Rows(rows)) => {
                let i = i as i64;
                if forward {
                    i.saturating_add(rows)
                } else {
                    i.saturating_sub(rows)
                }
            }
            Some(Reach::Distance(distance)) => match number(i) {
                None if is_start => first_peer[i] as i64,
                None => last_peer[i] as i64,
                Some(key) => {
                    let limit = if forward {
                        key + distance
                    } else {
                        key - distance
                    };
                    let value = |p: &usize| number(*p).expect("a numbered position");
                    if is_start {
                        // The first numbered position at or past the limit.
                        let index = numbered.partition_point(|p| value(p) < limit);
                        numbered.get(index).map_or(n, |&p| p as i64)
                    } else {
                        // The last numbered position up to the limit.
                        let index = numbered.partition_point(|p| value(p) <= limit);
                        index
                            .checked_sub(1)
                            .map_or(-1, |index| numbered[index] as i64)
                    }
                }
            },
        })
    };
    (0..partition.len())
        .map(|i| {
            let start = bound(i, &frame.start_bound, true)?.max(0);
            let end = bound(i, &frame.end_bound, false)?.min(n - 1);
            Ok((start, end))
        })
        .collect()
}
