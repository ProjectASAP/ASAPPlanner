//! `DecodeSamplesInRange`: the raw sample rows of one range, decoded from a
//! compressed-samples chain's windows (design doc
//! `compressed-sample-windows.md`). Its rows are those `TimeRange ← Scan`
//! returns over the same samples, so the exact operators above it answer
//! from them unchanged.
use super::*;
use crate::physical_planner::promql_rows::{decode_series_identity, SERIES_IDENTITY_COLUMN};
use crate::runtime::Scope;
use crate::summary_kernels::CompressedSamplesAccumulator;
use crate::AggregateCore;
use futures::TryStreamExt;
use std::collections::BTreeMap as Map;

/// Output rows per batch: series are emitted whole, a batch closing once it
/// holds this many rows.
const ROWS_PER_BATCH: usize = 8_192;

/// Where one output column of a decode comes from.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DecodedColumn {
    /// The chain row's key column `index` (the series identity, or a label
    /// the chain keeps).
    Key(usize),
    /// The sample's time.
    Time,
    /// The sample's value.
    Value,
    /// The label of that name, read from the series identity (NULL when
    /// the series has no such label).
    Label(String),
}

/// What a decode reads and emits.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DecodeSpec {
    /// The chain row's window-end column.
    pub(crate) window_end: usize,
    /// The chain row's state column.
    pub(crate) state: usize,
    /// The chain's tumbling-window width.
    pub(crate) width_ms: i64,
    /// The range read: `(T - offset_ms - range_ms, T - offset_ms]`.
    pub(crate) range_ms: i64,
    pub(crate) offset_ms: i64,
    /// An instant read: each series' newest sample in the range.
    pub(crate) instant: bool,
    /// Whether the chain's open window is read; otherwise only windows
    /// sealed by `T` (ending on the grid at or before it).
    pub(crate) reads_unsealed_window: bool,
    /// Each output column's source.
    pub(crate) columns: Vec<DecodedColumn>,
}

impl Operator {
    /// A decode of the compressed-samples chain rows `input` (`(window_end,
    /// keys, state)`) into the raw sample rows `output`, as `spec` says.
    /// Bad input when the window end is not a timestamp, the state is not
    /// compressed samples, or the columns do not match `output`'s types.
    pub(crate) fn decode_samples(
        input: SchemaRef,
        output: SchemaRef,
        spec: DecodeSpec,
    ) -> Result<Self, Error> {
        if plain(&input, spec.window_end)?.0 != &DataType::Timestamp {
            return Err(invalid("a decode reads its chain's window-end timestamps"));
        }
        if field(&input, spec.state)?.dtype != SummaryFamilyType::CompressedSamples {
            return Err(invalid("a decode reads compressed-samples states"));
        }
        if spec.width_ms <= 0 || spec.range_ms <= 0 || spec.offset_ms < 0 {
            return Err(invalid(
                "a decode has a positive window width and range, and no negative offset",
            ));
        }
        if spec.columns.len() != output.fields.len() {
            return Err(invalid("a decode names a source for each output column"));
        }
        for (column, field) in spec.columns.iter().zip(&output.fields) {
            let expected = match column {
                DecodedColumn::Key(index) => &crate::values::field(&input, *index)?.dtype,
                DecodedColumn::Time => &SummaryFamilyType::Plain(DataType::Timestamp),
                DecodedColumn::Value => &SummaryFamilyType::Plain(DataType::Float64),
                DecodedColumn::Label(_) => &SummaryFamilyType::Plain(DataType::Utf8),
            };
            if &field.dtype != expected {
                return Err(invalid(&format!(
                    "decoded column {} differs in type from its source",
                    field.name
                )));
            }
        }
        let labels = spec
            .columns
            .iter()
            .any(|column| matches!(column, DecodedColumn::Label(_)));
        let has_identity = spec.columns.iter().any(|column| {
            matches!(column, DecodedColumn::Key(index)
                if input.fields[*index].name == SERIES_IDENTITY_COLUMN)
        }) || input
            .fields
            .iter()
            .any(|field| field.name == SERIES_IDENTITY_COLUMN);
        if labels && !has_identity {
            return Err(invalid("a decode derives labels from a series identity"));
        }
        Ok(Self {
            kind: Kind::DecodeSamples(Box::new(spec)),
            inputs: vec![input],
            output,
        })
    }
}

/// One series' windows: its key values and each window's end and block.
struct Series {
    keys: Vec<Value>,
    identity: Option<Arc<str>>,
    windows: Vec<(i64, Arc<dyn AggregateCore>)>,
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::DecodeSamples(spec) = &operator.kind else {
        unreachable!()
    };
    let Scope::Query { evaluation_time_ms } = context.scope else {
        return Err(invalid("compressed samples are decoded at query time"));
    };
    let mut input = inputs
        .pop()
        .ok_or_else(|| invalid("decode input missing"))?;
    let input_schema = operator.inputs[0].clone();
    let end = evaluation_time_ms
        .checked_sub(spec.offset_ms)
        .ok_or_else(|| invalid("decode range overflows Int64"))?;
    let start = end
        .checked_sub(spec.range_ms)
        .ok_or_else(|| invalid("decode range overflows Int64"))?;
    // Windows ending after this are not sealed at `T`.
    let newest_sealed_end = evaluation_time_ms.div_euclid(spec.width_ms) * spec.width_ms;
    let identity_column = input_schema
        .fields
        .iter()
        .position(|field| field.name == SERIES_IDENTITY_COLUMN);
    let key_columns: Vec<usize> = (0..input_schema.fields.len())
        .filter(|&column| column != spec.window_end && column != spec.state)
        .collect();
    let output = operator.output.clone();
    let gathered = async move {
        // Every window's rows, grouped by series.
        let mut series: Map<Vec<u8>, Series> = Map::new();
        while let Some(batch) = input.next().await {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            for row in batch?.to_rows() {
                let Value::Timestamp(window_end) = row[spec.window_end] else {
                    return Err(invalid("a chain row's window end is a timestamp"));
                };
                if !spec.reads_unsealed_window && window_end > newest_sealed_end {
                    continue;
                }
                let Value::Summary { state, .. } = &row[spec.state] else {
                    return Err(invalid("a chain row holds a state"));
                };
                let mut key = Vec::new();
                for &column in &key_columns {
                    key.extend(row[column].key()?);
                }
                let entry = series.entry(key).or_insert_with(|| Series {
                    keys: row.clone(),
                    identity: identity_column.and_then(|column| match &row[column] {
                        Value::Utf8(identity) => Some(identity.clone()),
                        _ => None,
                    }),
                    windows: Vec::new(),
                });
                entry.windows.push((window_end, state.clone()));
            }
        }
        Ok(series.into_values().collect::<Vec<_>>())
    };
    Ok(futures::stream::once(gathered)
        .map(move |series| -> Result<_, Error> {
            let series = series?;
            let mut series = series.into_iter();
            let spec = spec.clone();
            let output = output.clone();
            Ok(futures::stream::iter(std::iter::from_fn(move || {
                let mut rows = Vec::new();
                for one in series.by_ref() {
                    if let Err(error) = decode_series(one, &spec, start, end, &mut rows) {
                        return Some(Err(error));
                    }
                    if rows.len() >= ROWS_PER_BATCH {
                        break;
                    }
                }
                if rows.is_empty() {
                    return None;
                }
                Some(Batch::try_new(output.clone(), rows))
            })))
        })
        .try_flatten()
        .boxed_local())
}

/// Appends to `rows` the samples of `series` in `(start, end]` (its newest
/// one for an instant read), each as an output row.
fn decode_series(
    mut series: Series,
    spec: &DecodeSpec,
    start: i64,
    end: i64,
    rows: &mut Vec<Vec<Value>>,
) -> Result<(), Error> {
    series.windows.sort_by_key(|(window_end, _)| *window_end);
    let blocks = series
        .windows
        .iter()
        .map(|(_, state)| {
            state
                .as_any()
                .downcast_ref::<CompressedSamplesAccumulator>()
                .ok_or_else(|| invalid("a decode reads compressed-samples states"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut samples = Vec::new();
    for block in blocks {
        samples.extend(
            block
                .samples_in(start, end)
                .map_err(|error| Error::Operator(error.to_string()))?,
        );
    }
    if spec.instant {
        samples.drain(..samples.len().saturating_sub(1));
    }
    if samples.is_empty() {
        return Ok(());
    }
    let labels = if spec
        .columns
        .iter()
        .any(|column| matches!(column, DecodedColumn::Label(_)))
    {
        let identity = series
            .identity
            .as_deref()
            .ok_or_else(|| invalid("a decode derives labels from a series identity"))?;
        Some(decode_series_identity(identity)?)
    } else {
        None
    };
    for (time, value) in samples {
        rows.push(
            spec.columns
                .iter()
                .map(|column| match column {
                    DecodedColumn::Key(index) => series.keys[*index].clone(),
                    DecodedColumn::Time => Value::Timestamp(time),
                    DecodedColumn::Value => Value::Float64(value),
                    DecodedColumn::Label(name) => labels
                        .as_ref()
                        .and_then(|labels| labels.get(name))
                        .map_or(Value::Null, |label| Value::Utf8(label.as_str().into())),
                })
                .collect(),
        );
    }
    Ok(())
}
