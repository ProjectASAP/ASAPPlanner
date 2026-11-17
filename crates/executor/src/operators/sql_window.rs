//! SQL analytic window functions (`f(…) OVER (PARTITION BY … ORDER BY …
//! frame)`): each row gets its function's value over its partition, as
//! DataFusion computes it.
use super::aggregate::kahan_inc;
use super::aggregate::running::{
    compensated_sum_value, extremum_replaces, integer_sum_value, RunningAvg,
};
use super::*;
use planner_types::ir::operator::{
    WindowFrame, WindowFrameBound, WindowFrameOffset, WindowFrameUnits, WindowFuncKind,
};
use planner_types::ir::scalar::ScalarValue;

/// One SQL window function over its input's partitions: `sum(v) OVER
/// (PARTITION BY k ORDER BY t ROWS 2 PRECEDING)` is `Sum` over column `v`,
/// partitioned by `k`, ordered by `t`, with that frame.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SqlWindowSpec {
    /// The function, as the IR's `WindowFuncKind` names it.
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
    pub fn sql_window(input: SchemaRef, spec: SqlWindowSpec, name: &str) -> Result<Self, Error> {
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
        let output = appended_schema(&input, name, dtype, nullable);
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
        let argument_type = spec
            .argument
            .map(|column| plain(&operator.inputs[0], column).map(|(dtype, _)| dtype.clone()))
            .transpose()?;
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
        // The results by row, their references for the encoder, and each
        // partition's peer groups and frames (buffers reused across
        // partitions, sized for the largest).
        let largest = largest_partition(&order, &group_of_row);
        workspace.grow(
            rows * (std::mem::size_of::<Value>() + std::mem::size_of::<&Value>())
                + largest * PARTITION_BYTES_PER_ROW,
        )?;
        let mut results = vec![Value::Null; rows];
        let mut buffers = PartitionBuffers::default();
        let mut work = Cooperative::new(&context);
        let mut start = 0;
        while start < rows {
            let group = group_of_row[order[start]];
            let end = start + order[start..].partition_point(|&row| group_of_row[row] == group);
            let partition = &order[start..end];
            evaluate_partition(
                spec,
                partition,
                &keys,
                &arguments,
                &argument_type,
                &mut buffers,
                &mut results,
            )?;
            work.checkpoint_rows(partition.len()).await?;
            start = end;
        }
        let column = crate::values::arrow_columns::encode(
            dtype,
            crate::values::arrow_columns::ColumnDepth::BatchColumn,
            &results.iter().collect::<Vec<_>>(),
        )?;
        batch.with_appended_column(operator.output.clone(), column)
    })
    .boxed_local())
}

/// The bytes a partition's working state holds per row at most: its
/// buffers ([`PartitionBuffers`]) and a MIN / MAX frame's candidates
/// ([`FrameAggregate`]).
const PARTITION_BYTES_PER_ROW: usize = 2 * std::mem::size_of::<usize>()
    + std::mem::size_of::<(i64, i64)>()
    + std::mem::size_of::<(i64, Value)>();

/// The rows of the largest partition: `order` holds the rows by partition.
fn largest_partition(order: &[usize], group_of_row: &[u32]) -> usize {
    order
        .chunk_by(|&a, &b| group_of_row[a] == group_of_row[b])
        .map(<[usize]>::len)
        .max()
        .unwrap_or(0)
}

/// One partition's working state, reused by the next: each position's
/// first and last peer, and its frame.
#[derive(Default)]
struct PartitionBuffers {
    first_peer: Vec<usize>,
    last_peer: Vec<usize>,
    frames: Vec<(i64, i64)>,
}

/// Each row's value of `spec` over one partition, written to `results` (by
/// row): `partition` holds its rows in window order; `keys` and
/// `arguments` (of `argument_type`) are indexed by row.
fn evaluate_partition(
    spec: &SqlWindowSpec,
    partition: &[usize],
    keys: &[Vec<Value>],
    arguments: &[Value],
    argument_type: &Option<DataType>,
    buffers: &mut PartitionBuffers,
    results: &mut [Value],
) -> Result<(), Error> {
    use WindowFuncKind as F;
    let n = partition.len();
    let peers = |a: usize, b: usize| {
        spec.order.iter().zip(keys).all(|(key, values)| {
            super::sort::compare_key(&values[partition[a]], &values[partition[b]], key).is_eq()
        })
    };
    // Each position's peer group: its first and last position.
    let PartitionBuffers {
        first_peer,
        last_peer,
        frames,
    } = buffers;
    first_peer.clear();
    first_peer.resize(n, 0);
    last_peer.clear();
    last_peer.resize(n, 0);
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
    let mut write = |position: usize, value: Value| results[partition[position]] = value;
    match &spec.function {
        F::RowNumber => {
            (0..n).for_each(|i| write(i, Value::Int64(i as i64 + 1)));
            return Ok(());
        }
        F::Rank => {
            (0..n).for_each(|i| write(i, Value::Int64(first_peer[i] as i64 + 1)));
            return Ok(());
        }
        F::DenseRank => {
            let mut rank = 0;
            for (i, &first) in first_peer.iter().enumerate() {
                if first == i {
                    rank += 1;
                }
                write(i, Value::Int64(rank));
            }
            return Ok(());
        }
        F::Lag | F::Lead => {
            let step = if spec.function == F::Lag {
                -spec.offset
            } else {
                spec.offset
            };
            for i in 0..n {
                let value = match (i as i64)
                    .checked_add(step)
                    .filter(|&j| j >= 0 && j < n as i64)
                {
                    Some(j) => argument(j as usize),
                    None => spec.default.clone(),
                };
                write(i, value);
            }
            return Ok(());
        }
        _ => {}
    }
    fill_frames(spec, partition, keys, first_peer, last_peer, frames)?;
    if matches!(spec.function, F::Sum | F::Avg | F::Count | F::Min | F::Max) {
        // `COUNT(*)` reads no argument: each row counts as a NULL value.
        let null = Value::Null;
        let at = |position: i64| arguments.get(partition[position as usize]).unwrap_or(&null);
        let mut aggregate = FrameAggregate::new(
            &spec.function,
            spec.argument.is_none(),
            *argument_type == Some(DataType::Int64),
        );
        for (i, &(start, end)) in frames.iter().enumerate() {
            write(i, aggregate.slide_to(start, end, at)?);
        }
        return Ok(());
    }
    for (i, &(start, end)) in frames.iter().enumerate() {
        let in_frame = |position: i64| (start..=end).contains(&position);
        let at = |position: i64| argument(position as usize);
        let value = match &spec.function {
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
            _ => unreachable!("rankings, offsets and aggregates returned above"),
        };
        write(i, value);
    }
    Ok(())
}

/// An aggregate window function (`SUM`, `AVG`, `COUNT`, `MIN`, `MAX`) over
/// a frame that slides: rows enter at its end and leave at its start, so a
/// partition's frames take O(n) steps in all while their starts and ends
/// only grow (every `ROWS` frame, and `RANGE` frames over the numbered
/// order keys); a frame that moves back is rebuilt. The values follow the
/// `Aggregate`'s rules ([`super::aggregate::running`]): over the frame
/// {1, NaN}, MAX is 1, as `SELECT max(v)` over those rows is.
///
/// `ROWS 2 PRECEDING` over 1, 2, 3, 4 holds {1}, {1, 2}, {1, 2, 3}, then 1
/// leaves and 4 enters: {2, 3, 4}.
struct FrameAggregate {
    function: WindowFuncKind,
    /// `COUNT(*)`: rows, not values, count.
    counts_rows: bool,
    /// Whether the argument is Int64 (summed exactly), not Float64.
    integer_argument: bool,
    /// The positions in the frame: `start..end`.
    start: i64,
    end: i64,
    /// Rows, and non-NULL values, in the frame.
    rows: i64,
    values: i64,
    /// The Int64 values' exact sum.
    integer_sum: i128,
    /// The finite Float64 values' compensated sum, and how many NaN and
    /// infinite values the frame holds besides.
    finite_sum: f64,
    compensation: f64,
    nan_values: i64,
    positive_infinities: i64,
    negative_infinities: i64,
    /// For a Float64 `AVG`: the values as the `Aggregate`'s `AVG` reduces
    /// them, valid while no value has left the frame since it was rebuilt.
    average: RunningAvg,
    /// Whether a value has left the frame since it was last rebuilt.
    values_left: bool,
    /// For MIN / MAX: the frame's positions (and values) that no later
    /// value replaces ([`extremum_replaces`]), in order; the first holds
    /// the extremum. MIN over 3, 1, 2 keeps (1, 2): 3 is replaced by 1.
    extremum_candidates: std::collections::VecDeque<(i64, Value)>,
}

impl FrameAggregate {
    fn new(function: &WindowFuncKind, counts_rows: bool, integer_argument: bool) -> Self {
        Self {
            function: function.clone(),
            counts_rows,
            integer_argument,
            start: 0,
            end: 0,
            rows: 0,
            values: 0,
            integer_sum: 0,
            finite_sum: 0.,
            compensation: 0.,
            nan_values: 0,
            positive_infinities: 0,
            negative_infinities: 0,
            average: RunningAvg::default(),
            values_left: false,
            extremum_candidates: Default::default(),
        }
    }

    /// Empty the frame, to grow again from position `at`.
    fn rebuild_at(&mut self, at: i64) {
        let mut candidates = std::mem::take(&mut self.extremum_candidates);
        candidates.clear();
        *self = Self {
            start: at,
            end: at,
            extremum_candidates: candidates,
            ..Self::new(&self.function, self.counts_rows, self.integer_argument)
        };
    }

    /// The function's value over the frame of positions `start..=end`
    /// (empty when `start > end`), `at` giving a position's value.
    fn slide_to<'v>(
        &mut self,
        start: i64,
        end: i64,
        at: impl Fn(i64) -> &'v Value,
    ) -> Result<Value, Error> {
        if start > end {
            return Ok(match self.function {
                WindowFuncKind::Count => Value::Int64(0),
                _ => Value::Null,
            });
        }
        let end = end + 1;
        if start < self.start || end < self.end || start >= self.end {
            self.rebuild_at(start);
        }
        while self.end < end {
            self.enter(self.end, at(self.end))?;
            self.end += 1;
        }
        while self.start < start {
            self.leave(self.start, at(self.start));
            self.start += 1;
        }
        if let Some(value) = self.value()? {
            return Ok(value);
        }
        // A Float64 sum that overflowed before values left: sum again.
        self.rebuild_at(start);
        while self.end < end {
            self.enter(self.end, at(self.end))?;
            self.end += 1;
        }
        Ok(self.value()?.expect("a rebuilt frame has its value"))
    }

    /// For MIN / MAX: whether it is MIN.
    fn extremum_is_min(&self) -> Option<bool> {
        match self.function {
            WindowFuncKind::Min => Some(true),
            WindowFuncKind::Max => Some(false),
            _ => None,
        }
    }

    /// Position `position`, of value `value`, enters at the frame's end.
    fn enter(&mut self, position: i64, value: &Value) -> Result<(), Error> {
        self.rows += 1;
        if matches!(value, Value::Null) {
            return Ok(());
        }
        self.values += 1;
        match value {
            Value::Int64(v) => self.integer_sum += i128::from(*v),
            Value::Float64(v) if v.is_nan() => self.nan_values += 1,
            Value::Float64(v) if *v == f64::INFINITY => self.positive_infinities += 1,
            Value::Float64(v) if *v == f64::NEG_INFINITY => self.negative_infinities += 1,
            Value::Float64(v) => {
                (self.finite_sum, self.compensation) =
                    kahan_inc(*v, self.finite_sum, self.compensation)
            }
            _ => {}
        }
        match (value, self.extremum_is_min()) {
            (Value::Float64(v), None) if self.function == WindowFuncKind::Avg => {
                self.average.add(*v)
            }
            (_, Some(min)) => {
                while let Some((_, kept)) = self.extremum_candidates.back() {
                    if extremum_replaces(kept, value, min)? {
                        break;
                    }
                    self.extremum_candidates.pop_back();
                }
                self.extremum_candidates
                    .push_back((position, value.clone()));
            }
            _ => {}
        }
        Ok(())
    }

    /// Position `position`, of value `value`, leaves at the frame's start.
    fn leave(&mut self, position: i64, value: &Value) {
        self.rows -= 1;
        if matches!(value, Value::Null) {
            return;
        }
        self.values -= 1;
        self.values_left = true;
        match value {
            Value::Int64(v) => self.integer_sum -= i128::from(*v),
            Value::Float64(v) if v.is_nan() => self.nan_values -= 1,
            Value::Float64(v) if *v == f64::INFINITY => self.positive_infinities -= 1,
            Value::Float64(v) if *v == f64::NEG_INFINITY => self.negative_infinities -= 1,
            Value::Float64(v) => {
                (self.finite_sum, self.compensation) =
                    kahan_inc(-*v, self.finite_sum, self.compensation)
            }
            _ => {}
        }
        if self
            .extremum_candidates
            .front()
            .is_some_and(|(kept, _)| *kept == position)
        {
            self.extremum_candidates.pop_front();
        }
    }

    /// The Float64 values' sum: NaN with a NaN or both infinities, else an
    /// infinity, else the finite values' sum. `None` when that sum
    /// overflowed and values left since: it may be finite again.
    fn float_sum(&self) -> Option<f64> {
        if self.values_left && !self.finite_sum.is_finite() {
            return None;
        }
        let finite = compensated_sum_value(self.finite_sum, self.compensation);
        let positive = self.positive_infinities > 0 || finite == f64::INFINITY;
        let negative = self.negative_infinities > 0 || finite == f64::NEG_INFINITY;
        Some(match (self.nan_values > 0, positive, negative) {
            (true, _, _) | (_, true, true) => f64::NAN,
            (_, true, _) => f64::INFINITY,
            (_, _, true) => f64::NEG_INFINITY,
            _ => finite,
        })
    }

    /// The function's value over the frame; `None` when the frame must be
    /// summed again ([`Self::float_sum`]).
    fn value(&self) -> Result<Option<Value>, Error> {
        use WindowFuncKind as F;
        let count = self.values as f64;
        Ok(Some(match self.function {
            F::Count if self.counts_rows => Value::Int64(self.rows),
            F::Count => Value::Int64(self.values),
            _ if self.values == 0 => Value::Null,
            F::Sum if self.integer_argument => integer_sum_value(self.integer_sum)?,
            F::Avg if self.integer_argument => Value::Float64(self.integer_sum as f64 / count),
            F::Avg if !self.values_left => Value::Float64(self.average.value()),
            F::Sum | F::Avg => {
                let Some(sum) = self.float_sum() else {
                    return Ok(None);
                };
                Value::Float64(match self.function {
                    // The `Aggregate`'s `AVG` before its sum overflows.
                    F::Avg if sum.is_finite() => {
                        self.finite_sum / count + self.compensation / count
                    }
                    _ => sum,
                })
            }
            F::Min | F::Max => self
                .extremum_candidates
                .front()
                .map_or(Value::Null, |(_, value)| value.clone()),
            _ => unreachable!("an aggregate window function"),
        }))
    }
}

/// Each position's frame in `partition`, as first and last position
/// (empty when first > last), into `frames`.
fn fill_frames(
    spec: &SqlWindowSpec,
    partition: &[usize],
    keys: &[Vec<Value>],
    first_peer: &[usize],
    last_peer: &[usize],
    frames: &mut Vec<(i64, i64)>,
) -> Result<(), Error> {
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
                    // NULL and NaN keys sort before or after every number,
                    // so the positions before the numbered run precede every
                    // limit and those after it follow every limit: over
                    // NULL, 8, 5 (NULLs first, descending), `1 PRECEDING`
                    // ends 8's frame at the NULL.
                    if is_start {
                        // The first numbered position at or past the limit,
                        // else the first after the numbered run.
                        let index = numbered.partition_point(|p| value(p) < limit);
                        match numbered.get(index) {
                            Some(&position) => position as i64,
                            None => numbered.last().map_or(n, |&last| last as i64 + 1),
                        }
                    } else {
                        // The last numbered position up to the limit, else
                        // the last before the numbered run.
                        let index = numbered.partition_point(|p| value(p) <= limit);
                        match index.checked_sub(1) {
                            Some(index) => numbered[index] as i64,
                            None => numbered.first().map_or(-1, |&first| first as i64 - 1),
                        }
                    }
                }
            },
        })
    };
    frames.clear();
    for i in 0..partition.len() {
        let start = bound(i, &frame.start_bound, true)?.max(0);
        let end = bound(i, &frame.end_bound, false)?.min(n - 1);
        frames.push((start, end));
    }
    Ok(())
}
