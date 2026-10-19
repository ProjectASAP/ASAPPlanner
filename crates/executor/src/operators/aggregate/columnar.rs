//! The exact `Aggregate` over column batches: a run-local group table
//! numbers each row's group, and each measure keeps one running state per
//! group, updated in row order. The result equals reducing each group's
//! rows in input order (the row-at-a-time operator).
use super::{kahan_inc, quantile, Reduction};
use crate::operators::common::{row_chunks, Workspace};
use crate::operators::group_table::GroupTable;
use crate::runtime::{Cooperative, Input, RunContext};
use crate::values::arrow_columns::{self, ColumnDepth};
use crate::values::{Batch, SchemaRef, Value};
use crate::{expressions::Expression, Error};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use arrow::array::{Array, ArrayRef, BooleanArray, Float64Array, Int64Array};
use futures::StreamExt;
use planner_types::ir::schema::{DataType, FieldDataType};

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

/// Prometheus `avg` as a running state: a compensated sum until it would
/// overflow, then an incremental mean (see `super::promql_avg`).
#[derive(Clone, Copy, Default)]
struct RunningAvg {
    sum: f64,
    compensation: f64,
    mean: f64,
    incremental: bool,
    count: u64,
}

impl RunningAvg {
    fn add(&mut self, value: f64) {
        if self.count == 0 {
            *self = Self {
                sum: value,
                count: 1,
                ..Self::default()
            };
            return;
        }
        self.count += 1;
        let count = self.count as f64;
        if !self.incremental {
            let (next, next_c) = kahan_inc(value, self.sum, self.compensation);
            if !next.is_infinite() {
                (self.sum, self.compensation) = (next, next_c);
                return;
            }
            self.incremental = true;
            self.mean = self.sum / (count - 1.);
            self.compensation /= count - 1.;
        }
        let q = (count - 1.) / count;
        (self.mean, self.compensation) =
            kahan_inc(value / count, q * self.mean, q * self.compensation);
    }
    fn value(&self) -> f64 {
        if self.count == 0 {
            return f64::NAN;
        }
        if self.incremental {
            self.mean + self.compensation
        } else {
            let count = self.count as f64;
            self.sum / count + self.compensation / count
        }
    }
}

/// One measure's running state per group.
enum MeasureStates {
    /// Rows selected per group.
    Count(Vec<i64>),
    /// Prometheus `sum`: a compensated sum and whether a value was seen.
    FloatSum(Vec<(f64, f64, bool)>),
    /// Prometheus `avg`.
    FloatAvg(Vec<RunningAvg>),
    /// An Int64 sum (or average): the exact sum and the value count.
    IntSum(Vec<(i128, u64)>),
    /// A Float64 MIN/MAX, NaN ignored when a number exists.
    FloatExtremum(Vec<Option<f64>>),
    /// MIN/MAX of another ordered type.
    ValueExtremum(Vec<Option<Value>>),
    /// The distinct non-NULL tuples: each (group, tuple) seen once, the
    /// tuples numbered by `tuples`, and each group's distinct count.
    Cardinality {
        tuples: GroupTable,
        seen: HashSet<(u32, u32)>,
        distinct: Vec<i64>,
    },
    /// Occurrences of each non-NULL identity per group, by (group,
    /// identity), the identities numbered by `identities`; and each group's
    /// total.
    Frequency {
        identities: GroupTable,
        counts: HashMap<(u32, u32), u64>,
        totals: Vec<u64>,
    },
    /// The non-NULL values per group.
    Quantile(Vec<Vec<f64>>),
}

impl MeasureStates {
    fn new(measure: &Reduction, input: &SchemaRef) -> Result<Self, Error> {
        let dtype = |column: usize| match &input.fields[column].dtype {
            FieldDataType::Plain(dtype) => Ok(dtype.clone()),
            _ => Err(invalid("plain value required")),
        };
        Ok(match measure {
            Reduction::Count => Self::Count(Vec::new()),
            Reduction::Sum(column) | Reduction::Avg(column)
                if dtype(*column)? == DataType::Int64 =>
            {
                Self::IntSum(Vec::new())
            }
            Reduction::Sum(_) => Self::FloatSum(Vec::new()),
            Reduction::Avg(_) => Self::FloatAvg(Vec::new()),
            Reduction::Min(column) | Reduction::Max(column) => {
                if dtype(*column)? == DataType::Float64 {
                    Self::FloatExtremum(Vec::new())
                } else {
                    Self::ValueExtremum(Vec::new())
                }
            }
            Reduction::Cardinality(columns) => Self::Cardinality {
                tuples: GroupTable::new(columns.clone()),
                seen: HashSet::new(),
                distinct: Vec::new(),
            },
            Reduction::FrequencyL2(column) | Reduction::FrequencyEntropy(column) => {
                Self::Frequency {
                    identities: GroupTable::new(vec![*column]),
                    counts: HashMap::new(),
                    totals: Vec::new(),
                }
            }
            Reduction::Quantile { .. } => Self::Quantile(Vec::new()),
        })
    }

    /// Room for `groups` groups; returns the bytes the new groups' states
    /// hold.
    fn ensure_groups(&mut self, groups: usize) -> usize {
        fn grow<T: Clone>(states: &mut Vec<T>, groups: usize, empty: T) -> usize {
            let added = groups.saturating_sub(states.len());
            states.resize(groups, empty);
            added * std::mem::size_of::<T>()
        }
        match self {
            Self::Count(v) => grow(v, groups, 0),
            Self::FloatSum(v) => grow(v, groups, (0., 0., false)),
            Self::FloatAvg(v) => grow(v, groups, RunningAvg::default()),
            Self::IntSum(v) => grow(v, groups, (0, 0)),
            Self::FloatExtremum(v) => grow(v, groups, None),
            Self::ValueExtremum(v) => grow(v, groups, None),
            Self::Cardinality { distinct, .. } => grow(distinct, groups, 0),
            Self::Frequency { totals, .. } => grow(totals, groups, 0),
            Self::Quantile(v) => grow(v, groups, Vec::new()),
        }
    }
}

/// Approximate bytes of one hash-set or hash-map entry, with its share of
/// the table's spare capacity.
const ENTRY_BYTES: usize = 32;

/// Whether row `row` of `array` is NULL, including a NULL-typed column's
/// rows (whose array has no null buffer).
fn logically_null(nulls: &Option<arrow::buffer::NullBuffer>, row: usize) -> bool {
    nulls.as_ref().is_some_and(|nulls| nulls.is_null(row))
}

/// Update `states` with the rows of `batch` whose group is `groups[row]`
/// and that `selected` keeps, keeping `workspace_bytes` (the bytes the
/// states hold) up to date: what they add, less what a replaced value
/// releases.
fn update_measure(
    states: &mut MeasureStates,
    measure: &Reduction,
    batch: &Batch,
    groups: &[u32],
    selected: impl Fn(usize) -> bool,
    workspace_bytes: &mut usize,
) -> Result<(), Error> {
    let floats = |column: usize| -> Result<Float64Array, Error> {
        batch
            .column(column)
            .as_any()
            .downcast_ref::<Float64Array>()
            .cloned()
            .ok_or_else(|| invalid("floating aggregate value required"))
    };
    match (states, measure) {
        (MeasureStates::Count(counts), _) => {
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) {
                    counts[group as usize] += 1;
                }
            }
        }
        (MeasureStates::FloatSum(sums), Reduction::Sum(column)) => {
            let values = floats(*column)?;
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) && values.is_valid(row) {
                    let (sum, c, seen) = &mut sums[group as usize];
                    (*sum, *c) = kahan_inc(values.value(row), *sum, *c);
                    *seen = true;
                }
            }
        }
        (MeasureStates::FloatAvg(averages), Reduction::Avg(column)) => {
            let values = floats(*column)?;
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) && values.is_valid(row) {
                    averages[group as usize].add(values.value(row));
                }
            }
        }
        (MeasureStates::IntSum(sums), Reduction::Sum(column) | Reduction::Avg(column)) => {
            let values = batch
                .column(*column)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| invalid("integer aggregate value required"))?;
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) && values.is_valid(row) {
                    let (sum, count) = &mut sums[group as usize];
                    *sum = sum
                        .checked_add(i128::from(values.value(row)))
                        .ok_or_else(|| invalid("integer aggregate overflow"))?;
                    *count += 1;
                }
            }
        }
        (MeasureStates::FloatExtremum(best), Reduction::Min(column) | Reduction::Max(column)) => {
            let values = floats(*column)?;
            let min = matches!(measure, Reduction::Min(_));
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) && values.is_valid(row) {
                    let value = values.value(row);
                    let slot = &mut best[group as usize];
                    *slot =
                        Some(slot.map_or(
                            value,
                            |old| {
                                if min {
                                    old.min(value)
                                } else {
                                    old.max(value)
                                }
                            },
                        ));
                }
            }
        }
        (MeasureStates::ValueExtremum(best), Reduction::Min(column) | Reduction::Max(column)) => {
            let FieldDataType::Plain(dtype) = &batch.schema().fields[*column].dtype else {
                return Err(invalid("plain value required"));
            };
            let values = arrow_columns::decode(
                batch.column(*column).as_ref(),
                dtype,
                ColumnDepth::BatchColumn,
            );
            let min = matches!(measure, Reduction::Min(_));
            for (row, &group) in groups.iter().enumerate() {
                let value = &values[row];
                if !selected(row) || matches!(value, Value::Null) {
                    continue;
                }
                let slot = &mut best[group as usize];
                let better = match slot {
                    None => true,
                    Some(old) => {
                        let order = value.compare(old)?;
                        if min {
                            order.is_lt()
                        } else {
                            order.is_gt()
                        }
                    }
                };
                if better {
                    // The replaced value's bytes are released.
                    let released = slot.as_ref().map_or(0, Value::bytes);
                    *workspace_bytes = *workspace_bytes - released + value.bytes();
                    *slot = Some(value.clone());
                }
            }
        }
        (
            MeasureStates::Cardinality {
                tuples,
                seen,
                distinct,
            },
            Reduction::Cardinality(columns),
        ) => {
            let before = tuples.memory_bytes();
            let ids = tuples.group_ids(batch)?;
            *workspace_bytes += tuples.memory_bytes() - before;
            let nulls = columns
                .iter()
                .map(|&c| batch.column(c).logical_nulls())
                .collect::<Vec<_>>();
            for (row, &group) in groups.iter().enumerate() {
                if !selected(row) || nulls.iter().any(|nulls| logically_null(nulls, row)) {
                    continue;
                }
                if seen.insert((group, ids[row])) {
                    distinct[group as usize] += 1;
                    *workspace_bytes += ENTRY_BYTES;
                }
            }
        }
        (
            MeasureStates::Frequency {
                identities,
                counts,
                totals,
            },
            Reduction::FrequencyL2(column) | Reduction::FrequencyEntropy(column),
        ) => {
            let before = identities.memory_bytes();
            let ids = identities.group_ids(batch)?;
            *workspace_bytes += identities.memory_bytes() - before;
            let array = batch.column(*column);
            let nulls = array.logical_nulls();
            let floats = array.as_any().downcast_ref::<Float64Array>();
            for (row, &group) in groups.iter().enumerate() {
                if !selected(row) || logically_null(&nulls, row) {
                    continue;
                }
                if floats.is_some_and(|floats| !floats.value(row).is_finite()) {
                    return Err(invalid(
                        "frequency aggregate requires a finite floating identity",
                    ));
                }
                let count = counts.entry((group, ids[row])).or_insert_with(|| {
                    *workspace_bytes += ENTRY_BYTES;
                    0
                });
                *count += 1;
                totals[group as usize] += 1;
            }
        }
        (MeasureStates::Quantile(values), Reduction::Quantile { column, .. }) => {
            let floats = floats(*column)?;
            for (row, &group) in groups.iter().enumerate() {
                if selected(row) && floats.is_valid(row) {
                    values[group as usize].push(floats.value(row));
                    *workspace_bytes += std::mem::size_of::<f64>();
                }
            }
        }
        _ => unreachable!("a measure's states match its reduction"),
    }
    Ok(())
}

/// Each group's value of one measure, in `order`.
fn finish_measure(
    states: MeasureStates,
    measure: &Reduction,
    dtype: &DataType,
    promql: bool,
    order: &[u32],
) -> Result<ArrayRef, Error> {
    let values: Vec<Value> = match states {
        MeasureStates::Count(counts) => order
            .iter()
            .map(|&g| Value::Int64(counts[g as usize]))
            .collect(),
        MeasureStates::FloatSum(sums) => order
            .iter()
            .map(|&g| {
                let (sum, c, seen) = sums[g as usize];
                if !seen && !promql {
                    Value::Null
                } else if sum.is_infinite() {
                    Value::Float64(sum)
                } else {
                    Value::Float64(sum + c)
                }
            })
            .collect(),
        MeasureStates::FloatAvg(averages) => order
            .iter()
            .map(|&g| {
                let average = averages[g as usize];
                if average.count == 0 && !promql {
                    Value::Null
                } else {
                    Value::Float64(average.value())
                }
            })
            .collect(),
        MeasureStates::IntSum(sums) => order
            .iter()
            .map(|&g| {
                let (sum, count) = sums[g as usize];
                if count == 0 && !promql {
                    Ok(Value::Null)
                } else if matches!(measure, Reduction::Avg(_)) {
                    Ok(Value::Float64(sum as f64 / count as f64))
                } else {
                    i64::try_from(sum)
                        .map(Value::Int64)
                        .map_err(|_| invalid("integer sum overflow"))
                }
            })
            .collect::<Result<_, _>>()?,
        MeasureStates::FloatExtremum(best) => order
            .iter()
            .map(|&g| best[g as usize].map_or(Value::Null, Value::Float64))
            .collect(),
        MeasureStates::ValueExtremum(mut best) => order
            .iter()
            .map(|&g| best[g as usize].take().unwrap_or(Value::Null))
            .collect(),
        MeasureStates::Cardinality { distinct, .. } => order
            .iter()
            .map(|&g| Value::Int64(distinct[g as usize]))
            .collect(),
        MeasureStates::Frequency {
            identities,
            counts,
            totals,
        } => {
            // Fold each group's counts in identity key order, as the row
            // operator's BTreeMap did.
            let mut rank = vec![0u32; identities.len()];
            for (position, identity) in identities.groups_in_key_order().into_iter().enumerate() {
                rank[identity as usize] = position as u32;
            }
            let mut counts_of_group: Vec<Vec<(u32, u64)>> = vec![Vec::new(); totals.len()];
            for ((group, identity), count) in counts {
                counts_of_group[group as usize].push((identity, count));
            }
            order
                .iter()
                .map(|&g| {
                    let group_counts = &mut counts_of_group[g as usize];
                    group_counts.sort_unstable_by_key(|(identity, _)| rank[*identity as usize]);
                    let total = totals[g as usize] as f64;
                    let mut result = 0.0_f64;
                    for &(_, count) in group_counts.iter() {
                        let count = count as f64;
                        if matches!(measure, Reduction::FrequencyL2(_)) {
                            result = result.hypot(count);
                        } else {
                            let probability = count / total;
                            result -= probability * probability.log2();
                        }
                    }
                    Value::Float64(result)
                })
                .collect()
        }
        MeasureStates::Quantile(mut values) => {
            let Reduction::Quantile { q, .. } = measure else {
                unreachable!()
            };
            order
                .iter()
                .map(|&g| Value::Float64(quantile(*q, std::mem::take(&mut values[g as usize]))))
                .collect()
        }
    };
    arrow_columns::encode(
        dtype,
        ColumnDepth::BatchColumn,
        &values.iter().collect::<Vec<_>>(),
    )
}

/// The exact `Aggregate`: the groups of `groups` in key order, each with
/// its `measures` (each over the rows its filter keeps).
pub(super) async fn aggregate(
    mut input: Input<'_, Batch>,
    groups: &[usize],
    measures: &[Reduction],
    filters: &[Option<Expression>],
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    let promql = input_schema.has_promql_series_identity();
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut workspace_bytes = 0usize;
    let mut table = GroupTable::new(groups.to_vec());
    let mut states = measures
        .iter()
        .map(|measure| MeasureStates::new(measure, input_schema))
        .collect::<Result<Vec<_>, _>>()?;
    // Each input batch stays under its producer's reservation while it is
    // reduced (it is not copied) and is dropped after it.
    while let Some(whole) = input.next().await {
        let whole = whole?;
        // A bounded number of rows at a time: the new groups' states are
        // charged before the next chunk adds more.
        for batch in row_chunks(&whole) {
            let ids = table.group_ids(&batch)?;
            work.checkpoint_rows(batch.num_rows()).await?;
            for state in &mut states {
                workspace_bytes += state.ensure_groups(table.len());
            }
            workspace.set_bytes(workspace_bytes + table.memory_bytes())?;
            for ((state, measure), filter) in states
                .iter_mut()
                .zip(measures)
                .zip(filters.iter().map(Some).chain(std::iter::repeat(None)))
            {
                // SQL FILTER keeps a row only when the predicate is true, not NULL.
                let mask = match filter.and_then(Option::as_ref) {
                    Some(filter) => {
                        let mask = filter.evaluate_batch(&batch, &DataType::Bool)?;
                        Some(
                            mask.as_any()
                                .downcast_ref::<BooleanArray>()
                                .cloned()
                                .ok_or_else(|| invalid("measure filter must be boolean"))?,
                        )
                    }
                    None => None,
                };
                update_measure(
                    state,
                    measure,
                    &batch,
                    &ids,
                    |row| {
                        mask.as_ref()
                            .is_none_or(|mask| mask.is_valid(row) && mask.value(row))
                    },
                    &mut workspace_bytes,
                )?;
                work.checkpoint_rows(batch.num_rows()).await?;
            }
            workspace.set_bytes(workspace_bytes + table.memory_bytes())?;
        }
    }
    // PromQL sums over an empty vector emit no sample.
    if table.len() == 0 && groups.is_empty() && !promql {
        table.add_empty_group()?;
        for state in &mut states {
            state.ensure_groups(1);
        }
    }
    let order = table.groups_in_key_order();
    let mut arrays = table.label_arrays(input_schema, &order)?;
    for ((state, measure), field) in states
        .into_iter()
        .zip(measures)
        .zip(&output.fields[groups.len()..])
    {
        let FieldDataType::Plain(dtype) = &field.dtype else {
            return Err(invalid("aggregate measures are plain"));
        };
        arrays.push(finish_measure(state, measure, dtype, promql, &order)?);
    }
    Batch::from_parts(
        output.clone(),
        arrays,
        vec![None; output.fields.len()],
        order.len(),
    )
}
